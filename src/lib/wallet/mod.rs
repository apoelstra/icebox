// ICBOC 3D
// Written in 2021 by
//   Andrew Poelstra <icboc@wpsoftware.net>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the CC0 Public Domain Dedication
// along with this software.
// If not, see <http://creativecommons.org/publicdomain/zero/1.0/>.
//

//! Wallet
//!
//! Support for the on-disk wallet format
//!

mod address;
mod chacha20;
mod crypt;
mod serialize;
mod txo;

use miniscript::{self, DescriptorTrait, TranslatePk2};
use miniscript::bitcoin::{self, util::bip32};

use self::serialize::Serialize;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::{
    cmp,
    fmt,
    io::{self, Read, Seek, Write},
};

use crate::{Dongle, Error};
pub use self::address::{Address, AddressInfo};
pub use self::txo::Txo;

/// Opaque cache of all scriptpubkeys the wallet is tracking
pub struct ScriptPubkeyCache {
    /// Scriptpubkeys we control
    spks: HashMap<bitcoin::Script, (u32, u32)>,
}

/// Wallet structure
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Wallet {
    /// Last blockheight the wallet considers confirmed and will not rescan
    pub block_height: u64,
    /// List of descriptors tracked by the wallet
    pub descriptors: Vec<Descriptor>,
    /// Set of outstanding addresses that have notes attached to them
    pub addresses: HashMap<bitcoin::Script, Address>,
    /// Set of TXOs owned by the wallet
    pub txos: HashMap<bitcoin::OutPoint, Txo>,
    /// Cache of keys we've gotten from the dongel
    pub key_cache: RefCell<HashMap<bip32::DerivationPath, bitcoin::PublicKey>>,
}

impl Wallet {
    /// Construct a new empty wallet
    pub fn new() -> Self { Self::default() }

    /// Iterator over all TXOs tracked by the wallet
    pub fn all_txos<'a, D: Dongle>(&'a self, dongle: &'a mut D) -> impl Iterator<Item=TxoInfo<'a>> {
        self.txos.keys().map(move |key| self.txo(dongle, *key).unwrap())
    }

    /// Helper fuction that caches keys from the Ledger and computes the
    /// scriptpubkey corresponding to an instantiated descriptor
    fn cache_key<D: Dongle>(
        key_cache: &RefCell<HashMap<bip32::DerivationPath, bitcoin::PublicKey>>,
        desc: &miniscript::Descriptor<miniscript::DescriptorPublicKey>,
        index: u32,
        dongle: &mut D,
    ) -> Result<bitcoin::Script, Error> {
        let dongle = RefCell::new(&mut *dongle);

        let copy = desc.derive(index);
        let inst = copy.translate_pk2(
            |key| dongle.borrow_mut().get_wallet_public_key(key, &mut *key_cache.borrow_mut())
        )?;
        Ok(inst.script_pubkey())
    }

    /// Adds a new descriptor to the wallet. Returns the number of new keys
    /// (i.e. it not covered by descriptors already in wallet) added.
    pub fn add_descriptor<D: Dongle>(
        &mut self,
        desc: miniscript::Descriptor<miniscript::DescriptorPublicKey>,
        low: u32,
        high: u32,
        dongle: &mut D,
    ) -> Result<usize, Error> {
        let mut existing_indices = HashSet::new();
        for d in &self.descriptors {
            if d.desc == desc {
                if d.low == low && d.high == high {
                    return Err(Error::DuplicateDescriptor);
                }
                for i in d.low..d.high {
                    existing_indices.insert(i);
                }
            }
        }

        let mut added_new = 0;
        for i in low..high {
            if !existing_indices.contains(&i) {
                added_new += 1;
                Wallet::cache_key(&self.key_cache, &desc, i, &mut *dongle)?;
            }
        }

        self.descriptors.push(Descriptor {
            desc: desc,
            low: low,
            high: high,
            next_idx: 0,
        });

        Ok(added_new)
    }

    /// Adds a new address to the wallet.
    pub fn add_address<'wallet, D: Dongle>(
        &'wallet mut self,
        dongle: &mut D,
        descriptor_idx: u32,
        wildcard_idx: Option<u32>,
        time: String,
        notes: String,
    ) -> Result<AddressInfo<'wallet>, Error> {
        let next_idx = &mut self.descriptors[descriptor_idx as usize].next_idx;
        let wildcard_idx = wildcard_idx.unwrap_or(*next_idx);
        *next_idx = cmp::max(*next_idx, wildcard_idx) + 1;

        let spk = Wallet::cache_key(
            &self.key_cache,
            &self.descriptors[descriptor_idx as usize].desc,
            wildcard_idx,
            &mut *dongle,
        )?;
        let spk_clone = spk.clone(); // sigh rust
        self.addresses.insert(spk, Address::new(descriptor_idx, wildcard_idx, time, notes));
        self.addresses[&spk_clone].info(self, dongle)
    }

    /// Iterator over all descriptors in the wallet, and their index
    pub fn descriptors<'a>(&'a self) -> impl Iterator<Item=(usize, &'a Descriptor)> {
        self.descriptors.iter().enumerate()
    }

    /// Gets the set of TXOs associated with a particular descriptor
    pub fn txos_for<'a>(&'a self, descriptor_idx: usize) -> HashSet<&'a Txo> {
        self.txos.values().filter(|txo| txo.descriptor_idx() as usize == descriptor_idx).collect()
    }

    /// Looks up a specific TXO
    pub fn txo<'a, D: Dongle>(
        &'a self,
        dongle: &mut D,
        outpoint: bitcoin::OutPoint,
    ) -> Result<TxoInfo<'a>, Error> {
        let txo = match self.txos.get(&outpoint) {
            Some(txo) => txo,
            None => return Err(Error::TxoNotFound(outpoint)),
        };
        let descriptor = self.descriptors[txo.descriptor_idx() as usize].desc.derive(txo.wildcard_idx());

        let dongle = RefCell::new(&mut *dongle);

        let inst = descriptor.translate_pk2(
            |key| dongle.borrow_mut().get_wallet_public_key(key, &mut *self.key_cache.borrow_mut())
        )?;
        let spk = inst.script_pubkey();
        Ok(TxoInfo {
            txo: txo,
            address: inst.address(bitcoin::Network::Bitcoin).expect("getting bitcoin address"),
            descriptor: &self.descriptors[txo.descriptor_idx() as usize],
            address_info: self.addresses.get(&spk),
        })
    }

    /// Returns an opaque object the wallet can use to recognize its own scriptpubkeys
    pub fn script_pubkey_cache<D: Dongle>(
        &mut self,
        dongle: &mut D,
    ) -> Result<ScriptPubkeyCache, Error> {
        let mut map = HashMap::new();
        for (didx, desc) in self.descriptors.iter().enumerate() {
            for widx in desc.low..desc.high {
                let spk = Wallet::cache_key(&mut self.key_cache, &desc.desc, widx, &mut *dongle)?;
                map.insert(spk, (didx as u32, widx as u32));
            }
        }

        Ok(ScriptPubkeyCache {
            spks: map,
        })
    }

    /// Scans a block for wallet-relevant information. Returns two sets, one of
    /// received coins and one of spent coins
    pub fn scan_block(
        &mut self,
        block: &bitcoin::Block,
        height: u64,
        cache: &mut ScriptPubkeyCache,
    ) -> Result<(HashSet<bitcoin::OutPoint>, HashSet<bitcoin::OutPoint>), Error> {
        let mut received = HashSet::new();
        let mut spent = HashSet::new();

        for tx in &block.txdata {
            for (vout, output) in tx.output.iter().enumerate() {
                if let Some((didx, widx)) = cache.spks.get(&output.script_pubkey) {
                    let outpoint = bitcoin::OutPoint::new(tx.txid(), vout as u32);
                    let new_txo = Txo::new(*didx, *widx, outpoint, output.value, height);
                    self.txos.insert(outpoint, new_txo);
                    received.insert(outpoint);
                }
            }

            for input in &tx.input {
                if let Some(txo) = self.txos.get_mut(&input.previous_output) {
                    txo.set_spent(tx.txid(), height);
                    spent.insert(input.previous_output);
                }
            }
        }

        Ok((received, spent))
    }

    /// Read a wallet in encrypted form
    pub fn from_reader<R: Read + Seek>(r: R, key: [u8; 32]) -> io::Result<Self> {
        let reader = self::crypt::CryptReader::new(key, r)?;
        Self::read_from(reader)
    }

    /// Write out the wallet in encrypted form
    pub fn write<W: Write>(&self, w: W, key: [u8; 32], nonce: [u8; 12]) -> io::Result<()> {
        let mut writer = self::crypt::CryptWriter::new(key, nonce, w);
        writer.init()?;
        self.write_to(&mut writer)?;
        writer.finalize()?;
        Ok(())
    }
}

impl Serialize for Wallet {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        self.block_height.write_to(&mut w)?;
        self.descriptors.write_to(&mut w)?;
        self.addresses.write_to(&mut w)?;
        self.txos.write_to(&mut w)?;
        self.key_cache.borrow().write_to(w)
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        Ok(Wallet {
            block_height: Serialize::read_from(&mut r)?,
            descriptors: Serialize::read_from(&mut r)?,
            addresses: Serialize::read_from(&mut r)?,
            txos: Serialize::read_from(&mut r)?,
            key_cache: RefCell::new(Serialize::read_from(r)?),
        })
    }
}

/// A descriptor held in the wallet
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// The underlying descriptor
    pub desc: miniscript::Descriptor<miniscript::DescriptorPublicKey>,
    /// The first (inclusive) index to instantiate
    pub low: u32,
    /// The last (exclusize) index to instantiate
    pub high: u32,
    /// The next unused index at which to instantiate this descriptor
    pub next_idx: u32,
}

impl Serialize for Descriptor {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        self.desc.write_to(&mut w)?;
        self.low.write_to(&mut w)?;
        self.high.write_to(&mut w)?;
        self.next_idx.write_to(w)
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        Ok(Descriptor {
            desc: Serialize::read_from(&mut r)?,
            low: Serialize::read_from(&mut r)?,
            high: Serialize::read_from(&mut r)?,
            next_idx: Serialize::read_from(r)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A structure containing information about a txo tracked by the wallet
pub struct TxoInfo<'wallet> {
    txo: &'wallet Txo,
    descriptor: &'wallet Descriptor,
    address: bitcoin::Address,
    address_info: Option<&'wallet Address>,
}

impl<'wallat> TxoInfo<'wallat> {
    /// Accessor for the value of this TXO
    pub fn value(&self) -> u64 {
        self.txo.value()
    }

    /// Whether the TXO has been spent or not
    pub fn is_unspent(&self) -> bool {
        self.txo.spent_height().is_none()
    }
}

impl<'wallat> Ord for TxoInfo<'wallat> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        fn sort_key<'a>(obj: &TxoInfo<'a>) -> impl Ord {
            (obj.txo.height(), obj.txo.descriptor_idx(), obj.txo.wildcard_idx(), obj.txo.outpoint())
        }
        sort_key(self).cmp(&sort_key(other))
    }
}

impl<'wallat> PartialOrd for TxoInfo<'wallat> {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<'wallat> fmt::Display for TxoInfo<'wallat> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{{ outpoint: \"{}\", value: \"{}\", height: {}, descriptor: \"{}\", index: {}",
            self.txo.outpoint(),
            bitcoin::Amount::from_sat(self.txo.value()),
            self.txo.height(),
            self.descriptor.desc,
            self.txo.wildcard_idx(),
        )?;
        if let Some(txid) = self.txo.spending_txid() {
            write!(f, ", spent_by: \"{}\"", txid)?;
        }
        if let Some(height) = self.txo.spent_height() {
            write!(f, ", spent_height: {}", height)?;
        }
        if let Some(addrinfo) = self.address_info {
            write!(f, ", address_created_at: \"{}\"", addrinfo.create_time())?;
            write!(f, ", notes: \"{}\"", addrinfo.notes())?;
        }
        f.write_str("}")
    }
}
