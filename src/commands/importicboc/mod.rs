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

//! `importicboc`
//!
//! Imports wallet data from an ICBOC 1D wallet
//!

mod aes;

use anyhow::{self, Context};
use crate::rpc;
use icboc::Dongle;
use miniscript::bitcoin::util::bip32;
use serde::Deserialize;
use std::{
    io::Read,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

/// Magic string indicating start of ICBOC 1D wallet 
pub const MAGIC: [u8; 8] = [0x31, 0x60, 0xf9, 0x0d, 0xaa, 0xe5, 0x00, 0x01];
/// Size, in bytes, of an AES-CTR-encrypted data block.
pub const ENCRYPTED_ENTRY_SIZE: usize = 352;
/// Size, in bytes, of a data block.
pub const DECRYPTED_ENTRY_SIZE: usize = 336;


/// Gets information
pub struct ImportIcboc;

/// Gets information
#[derive(Deserialize)]
pub struct Options {
    /// Filename of the ICBOC 1D wallet to import
    file: PathBuf,
}

impl super::Command for ImportIcboc {
    type Options = Options;

    fn execute<D: Dongle, P: AsRef<Path>>(
        options: Self::Options,
        wallet_path: P,
        _bitcoind: &rpc::Bitcoind,
        dongle: &mut D,
    ) -> anyhow::Result<()> {
        let (key, nonce) = super::get_wallet_key_and_nonce(dongle)?;
        let mut wallet = super::open_wallet(&wallet_path, key)?;
        let icboc_name = options.file.to_string_lossy().into_owned();

        let icboc_meta = fs::metadata(&options.file)
            .with_context(|| format!("getting metadata for ICBOC 1D wallet {}", icboc_name))?;
        let icboc_size = icboc_meta.len() as usize;
        if icboc_size % ENCRYPTED_ENTRY_SIZE != 12 {
            return Err(anyhow::Error::msg(format!("bad ICBOC 1D wallet size {}", icboc_size)));
        }
        let n_entries = icboc_size / ENCRYPTED_ENTRY_SIZE;

        let mut fh = fs::File::open(&options.file)
            .with_context(|| format!("opening ICBOC 1D wallet {}", icboc_name))?;
        let mut magic = [0; 8];
        fh.read_exact(&mut magic).context("reading magic bytes")?;
        if magic != MAGIC {
            return Err(anyhow::Error::msg(format!("invalid ICBOC 1D wallet (magic bytes {:?}, expected {:?})", magic, MAGIC)));
        }

        fh.read_exact(&mut magic[0..4]).context("reading account number bytes")?;
        if &magic[0..4] != &[0; 4] {
            return Err(anyhow::Error::msg("account number was not 0, is this a real icboc wallet?"));
        }

        println!("Found ICBOC 1D wallet with {} entries. Fetching that many keys from the Ledger.", n_entries);

        // 1. Import descriptor
        let master_xpub = dongle.get_master_xpub()
            .context("getting master xpub")?;
        let desc_idx = wallet.descriptors.len();
        let desc = miniscript::Descriptor::from_str(
            &format!("pkh({}/44h/0h/0h/2h/*h)", master_xpub)
        ).expect("well-formed descriptor");

        if n_entries >= (1 << 31) {
            return Err(anyhow::Error::msg(format!("cannot import wallet with {} entries (max 2^31)", n_entries)));
        }
        wallet.add_descriptor(
            desc,
            0,
            n_entries as u32,
            &mut *dongle,
        ).with_context(|| "importing descriptor")?;

        // 2. Read all entries
        println!("Imported descriptor. Importing entries.");
        for i in 0..n_entries {
            let mut enc_entry = [0; ENCRYPTED_ENTRY_SIZE];
            fh.read_exact(&mut enc_entry).with_context(|| format!("reading entry {}", i))?;

            let (iv, entry) = enc_entry.split_at_mut(16);
            let encryption_key = dongle.get_public_key(&[
                    bip32::ChildNumber::Hardened { index: 44 },
                    bip32::ChildNumber::Hardened { index: 0 },
                    bip32::ChildNumber::Hardened { index: 0 },
                    bip32::ChildNumber::Hardened { index: 3 },
                    bip32::ChildNumber::Hardened { index: i as u32 },
                ],
                false,
            )?.chain_code;
	    let decrypted_entry = self::aes::aes256_decrypt_ctr(encryption_key, iv, entry);

            if decrypted_entry != &[0; DECRYPTED_ENTRY_SIZE] {
                let time = String::from_utf8(decrypted_entry[164..188].to_owned())
                    .with_context(|| format!("decoding timestamp from entry {}", i))?;
                let notes = {
                    let mut endidx = 252;
                    while endidx <= decrypted_entry.len() && decrypted_entry[endidx] != 0 {
                        endidx += 1;
                    } 
                    String::from_utf8(decrypted_entry[252..endidx].to_owned())
                        .with_context(|| format!("decoding notes from entry {}", i))?
                };
                wallet.add_address(&mut *dongle, desc_idx as u32, Some(i as u32), time, notes)
                    .with_context(|| format!("importing address for entry {}", i))?;
            }

            if i % 25 == 24 {
                println!("Done {}/{}", i + 1, n_entries);
            }
        }

        // 3. Save out
        super::save_wallet(&wallet, wallet_path, key, nonce)
            .with_context(|| format!("saving wallet after import"))?;

        println!("Imported entries from wallet. You should now run `rescan`.");
        return Ok(());
    }
}
