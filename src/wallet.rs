// ICBOC
// Written in 2017 by
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

//! # Wallet
//!
//! Support for the "wallet" which is really more of an audit log
//!

use bitcoin::{Address, Script, Transaction, TxOut, SigHashType};
use bitcoin::blockdata::script;
use bitcoin::network::constants::Network;
use bitcoin::util::hash::Sha256dHash;
use byteorder::{ByteOrder, ReadBytesExt, WriteBytesExt, BigEndian};
use crypto::aes;
use hex::ToHex;
use secp256k1::{self, Secp256k1};
use std::{fmt, io, fs, str};
use std::str::FromStr;
use std::io::{Read, Write};
use time;

use constants::wallet::{DECRYPTED_ENTRY_SIZE, ENCRYPTED_ENTRY_SIZE, MAGIC, MAGIC_TESTNET, MAX_USER_ID_BYTES, MAX_NOTE_BYTES, CHANGE_DUST};
use dongle::Dongle;
use error::Error;
use util::{hash_sha256, convert_compact_to_secp};
use spend;

/// List of purposes that we use BIP32 keys
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum KeyPurpose {
    /// A Bitcoin address
    Address,
    /// The chaincode is an AES key
    AesKey,
    // TODO p2contract nonce (need Ledger support)
}

/// Obtain a BIP32 path corresponding to the appropriate key
pub fn bip32_path(network: Network, account: u32, purpose: KeyPurpose, index: u32) -> [u32; 5] {
    let pp_index = match purpose {
        // 0 and 1 would correspond to "normal" or "change" in BIP44;
        // we deliberately avoid these indices to avoid standard BIP44
        // wallets overlapping with us, although we observe that using
        // hardened keys is already sufficient for this
        KeyPurpose::Address       => 2,
        KeyPurpose::AesKey        => 3,
    };
    let coin_type = match network {
        Network::Bitcoin => 0x80000000,
        Network::Testnet => 0x80000001,
        Network::Regtest => 0x80000001,
    };
    [0x8000002c, coin_type, 0x80000000 | account, 0x80000000 | pp_index, 0x80000000 | index]
}

// This whole encryption business should be done on the dongle
/// Helper function to encrypt an entry
fn encrypt<D: Dongle>(dongle: &mut D, network: Network, account: u32, index: usize, input: &[u8], output: &mut [u8]) -> Result<(), Error> {
    let key = dongle.get_public_key(&bip32_path(network, account, KeyPurpose::AesKey, index as u32), false)?;
    let iv = dongle.get_random(16)?;
    let mut encryptor = aes::ctr(aes::KeySize::KeySize256, &key.chaincode[..], &iv);
    output[0..16].copy_from_slice(&iv);
    encryptor.process(input, &mut output[16..]);
    Ok(())
}

/// Helper function to decrypt an entry
fn decrypt<D: Dongle>(dongle: &mut D, network: Network, account: u32, index: usize, input: &[u8], output: &mut [u8]) -> Result<(), Error> {
    let key = dongle.get_public_key(&bip32_path(network, account, KeyPurpose::AesKey, index as u32), false)?;
    let iv = &input[0..16];
    let mut encryptor = aes::ctr(aes::KeySize::KeySize256, &key.chaincode[..], iv);
    encryptor.process(&input[16..], output);
    Ok(())
}

/// Extra information needed when updating an entry
pub enum Update<'a> {
    /// This entry should be labelled etc but has not yet received any coins
    Unused(String),
    /// This entry is created as change so is immediately used
    Change(&'a Transaction, u32)
}

/// Structure representing an encrypted wallet
pub struct EncryptedWallet {
    network: Network,
    account: u32,
    entries: Vec<[u8; ENCRYPTED_ENTRY_SIZE]>
}

impl EncryptedWallet {
    /// Construct a new empty wallet with the given account number
    pub fn new<D: Dongle>(dongle: &mut D, network: Network, account: u32, n_entries: usize) -> Result<EncryptedWallet, Error> {
        let mut ret = EncryptedWallet {
            network: network,
            account: account,
            entries: Vec::with_capacity(n_entries)
        };

        dongle.set_network(network)?;

        for i in 0..n_entries {
            info!("Encrypting zeroes for key {}", i);
            let mut block = [0; ENCRYPTED_ENTRY_SIZE];
            let zeroes = [0; DECRYPTED_ENTRY_SIZE];
            encrypt(dongle, network, account, ret.entries.len(), &zeroes, &mut block)?;
            ret.entries.push(block);
        }

        Ok(ret)
    }

    /// Extends the number of entries in the wallet
    pub fn extend<D: Dongle>(&mut self, dongle: &mut D, n_entries: usize) -> Result<(), Error> {
        if n_entries <= self.entries.len() {
            return Ok(());
        }
        for i in self.entries.len()..n_entries {
            info!("Encrypting zeroes for key {}", i);
            let mut block = [0; ENCRYPTED_ENTRY_SIZE];
            let zeroes = [0; DECRYPTED_ENTRY_SIZE];
            encrypt(dongle, self.network, self.account, i, &zeroes, &mut block)?;
            self.entries.push(block);
        }
        Ok(())
    }

    /// Saves out the wallet to a file
    pub fn save(&self, filename: &str) -> Result<(), Error> {
        let mut temp_name = filename.to_owned();
        temp_name.push_str(".0");
        let fh = fs::File::create(&temp_name)?;
        let mut buf = io::BufWriter::new(fh);
        if self.network == Network::Testnet {
            buf.write_u64::<BigEndian>(MAGIC_TESTNET)?;
        } else {
            buf.write_u64::<BigEndian>(MAGIC)?;
        }
        buf.write_u32::<BigEndian>(self.account)?;
        for data in &self.entries {
            buf.write(&data[..])?;
        }
        fs::rename(&temp_name, filename)?;
        info!("Saved wallet to {}", filename);
        Ok(())
    }

    /// Loads a wallet from a file
    pub fn load<D: Dongle>(dongle: &mut D, filename: &str) -> Result<EncryptedWallet, Error> {
        let meta = fs::metadata(filename)?;
        let size = meta.len() as usize;

        if size % ENCRYPTED_ENTRY_SIZE != 12 {
            return Err(Error::WalletWrongSize(size));
        }

        let mut ret = EncryptedWallet {
            network: Network::Bitcoin,
            account: 0,
            entries: Vec::with_capacity(size / ENCRYPTED_ENTRY_SIZE)
        };

        let mut fh = fs::File::open(filename)?;
        let magic = fh.read_u64::<BigEndian>()?;
        match magic {
            MAGIC => {},
            MAGIC_TESTNET => { ret.network = Network::Testnet; }
            _ => { return Err(Error::WalletWrongMagic(magic)); }
        }

        ret.account = fh.read_u32::<BigEndian>()?;
        for _ in 0..ret.entries.capacity() {
            let mut entry = [0; ENCRYPTED_ENTRY_SIZE];
            fh.read_exact(&mut entry)?;
            ret.entries.push(entry);
        }

        dongle.set_network(ret.network)?;

        Ok(ret)
    }

    /// Scan the wallet for the first unused index
    pub fn next_unused_index<D: Dongle>(&self, dongle: &mut D) -> Result<usize, Error> {
        for i in 0..self.entries.len() {
            let entry = self.lookup(dongle, i)?;
            if entry.state == EntryState::Unused {
                return Ok(entry.index)
            }
        }
        Err(Error::WalletFull)
    }

    /// Accessor for the encrypted data in a wallet
    pub fn lookup<D: Dongle>(&self, dongle: &mut D, index: usize) -> Result<Entry, Error> {
        if index + 1 > self.entries.len() {
            return Err(Error::EntryOutOfRange(index));
        }

        Entry::decrypt_and_verify(dongle, self.network, self.account, index, &self.entries[index])
    }

    /// Does a linear scan for a base58-encoded address
    pub fn search<D: Dongle>(&self, dongle: &mut D, address: &str) -> Result<Entry, Error> {
        for (i, entry) in self.entries.iter().enumerate() {
            let key = dongle.get_public_key(&bip32_path(self.network, self.account, KeyPurpose::Address, i as u32), false)?;
            if key.b58_address == address {
                return Entry::decrypt_and_verify(dongle, self.network, self.account, i, entry);
            }
        }
        Err(Error::AddressNotFound)
    }

    /// Display an address on the Ledger screen and make the user click "confirm"
    pub fn display<'a, D: Dongle>(&self, dongle: &mut D, index: usize) -> Result<(), Error> {
        let path = bip32_path(self.network, self.account, KeyPurpose::Address, index as u32);
        dongle.get_public_key(&path, true)?;
        Ok(())
    }

    /// Update an address entry to indicate that it is in use
    pub fn update<'a, D: Dongle>(&mut self, dongle: &mut D, index: usize, user: String, blockhash: Sha256dHash, data: Update<'a>) -> Result<Entry, Error> {
        if user.as_bytes().len() > MAX_USER_ID_BYTES {
            return Err(Error::UserIdTooLong(user.as_bytes().len(), MAX_USER_ID_BYTES));
        }

        let timestr = time::strftime("%F %T%z", &time::now()).unwrap();
        assert_eq!(timestr.bytes().len(), 24);
        let mut timesl = [0; 24];
        timesl.clone_from_slice(timestr.as_bytes());
        let mut block = [0; 32];
        block.clone_from_slice(&blockhash[..]);

        let path = bip32_path(self.network, self.account, KeyPurpose::Address, index as u32);
        let key = dongle.get_public_key(&path, false)?;

        let state;
        let note;
        let mut trusted_input = [0; 56];
        let mut txid = [0; 32];
        let vout;
        let amount;
        match data {
            Update::Unused(note_) => {
                state = EntryState::Valid;
                if note_.as_bytes().len() > MAX_NOTE_BYTES {
                    return Err(Error::NoteTooLong(note_.as_bytes().len(), MAX_NOTE_BYTES));
                }
                note = note_;
                vout = 0;
                amount = 0;
            }
            Update::Change(tx, vout_) => {
                let hash = tx.txid();
                state = EntryState::Received;
                note = format!("change of {}", hash);
                let trusted_input_ = dongle.get_trusted_input(tx, vout_)?;
                trusted_input.copy_from_slice(&trusted_input_[..]);
                txid.copy_from_slice(&hash[..]);
                vout = vout_;
                amount = tx.output[vout as usize].value;
            }
        }
        let entry = Entry {
            state: state,
            bip32_path: path,
            spent: false,
            trusted_input: trusted_input,
            address: Address::from_str(&key.b58_address)?,
            index: index,
            txid: txid,
            vout: vout,
            amount: amount,
            date: timesl,
            user: user,
            blockhash: block,
            note: note
        };

        self.entries[index] = entry.sign_and_encrypt(dongle, self.network, self.account, index)?;

        Ok(entry)
    }

    /// Does a linear scan to compute the total wallet balance
    pub fn get_balance<D: Dongle>(&self, dongle: &mut D) -> Result<u64, Error> {
        let mut balance = 0;
        for i in 0..self.entries.len() {
            let entry = self.lookup(dongle, i)?;
            if !entry.spent {
                balance += entry.amount;
            }
        }
        Ok(balance)
    }

    /// Process a transaction which claims to send coins to this wallet,
    /// finding all output which send coins to us
    pub fn receive<D: Dongle>(&mut self, dongle: &mut D, tx: &Transaction) -> Result<(), Error> {
        let txid = tx.txid();

        for i in 0..self.entries.len() {
            let mut entry = self.lookup(dongle, i)?;
            // Catch Unused early because otherwise we'll error out trying
            // to parse a bunch of zeroes as meaningful data
            if entry.state == EntryState::Unused {
                info!("Skipping unused entry {} (use `getaddress {}` to mark it used).", i, i);
                continue;
            }
            let spk = entry.address.script_pubkey();
            for (vout, out) in tx.output.iter().enumerate() {
                if out.script_pubkey == spk {
                    info!("Receive to entry {}. Amount {}, outpoint {}:{}!", i, out.value, txid, vout);
                    // Before updating anything check the state of the entry to see if this is allowed.
                    match entry.state {
                        EntryState::Unused => unreachable!(),
                        EntryState::Invalid => {
                            error!("Entry has a bad signature (wallet is corrupted?). Rejecting this transaction.");
                            return Err(Error::BadSignature);
                        }
                        EntryState::Received => {
                            if &entry.txid[..] == &txid[..] && entry.vout == vout as u32 {
                                warn!("Have receive of {}:{} already recorded", txid, vout);
                            } else {
                                error!("Entry has already received coins. Rejecting this transaction.");
                                error!("(You can work around this by creating another wallet with account {},", self.account);
                                error!("doing `getaddress {}` on it, and sweeping the coins to this one.)", i);
                                return Err(Error::DoubleReceive);
                            }
                        }
                        EntryState::Valid => {
                            // Ok, update
                            let trusted_input = dongle.get_trusted_input(tx, vout as u32)?;
                            entry.state = EntryState::Received;
                            entry.trusted_input.copy_from_slice(&trusted_input[..]);
                            entry.txid.copy_from_slice(&txid[..]);
                            entry.vout = vout as u32;
                            entry.amount = out.value;
                            self.entries[i] = entry.sign_and_encrypt(dongle, self.network, self.account, i)?;
                        }
                    }
                }
            } // end txo loop
        } // end entries loop
        Ok(())
    }

    /// Mark an address as having been spent
    pub fn mark_spent<D: Dongle>(&mut self, dongle: &mut D, index: usize) -> Result<(), Error> {
        let mut entry = self.lookup(dongle, index)?;
        entry.spent = true;
        self.entries[index] = entry.sign_and_encrypt(dongle, self.network, self.account, index)?;
        Ok(())
    }

    /// Re-encrypts the entire wallet so that everything will appear updated,
    /// to resist attacks where an attacker determines "used" wallets by
    /// obtaining an empty copy and seeing which entries have changed
    pub fn rerandomize<D: Dongle>(&mut self, dongle: &mut D) -> Result<(), Error> {
        for i in 0..self.entries.len() {
            let mut tmp = [0; DECRYPTED_ENTRY_SIZE];
            decrypt(dongle, self.network, self.account, i, &self.entries[i], &mut tmp)?;
            encrypt(dongle, self.network, self.account, i, &tmp, &mut self.entries[i])?;
        }
        Ok(())
    }

    /// Scan the wallet finding funds in excess of `total_amount` as well
    /// as the next available unused address for change
    pub fn get_inputs_and_change<D: Dongle>(&self, dongle: &mut D, fee_rate: u64, spend: &mut spend::Spend) -> Result<(), Error> {
        let mut found_amount = 0;
        let mut found_change = false;

        // (Over)estimate tx size for fee accounting purposes
        let mut size_bytes = (13 + ((spend.output.len() + 1) * 34)) as u64;
        let mut total_amount = 0;
        for output in &spend.output {
            total_amount += output.value;
        }

        for i in 0..self.entries.len() {
            let entry = self.lookup(dongle, i)?;
            // Check for change
            match entry.state {
                EntryState::Unused => {
                    if !found_change {
                        spend.output.push(TxOut {
                            script_pubkey: entry.address.script_pubkey(),
                            value: 0
                        });
                        spend.change_path = bip32_path(self.network, self.account, KeyPurpose::Address, i as u32);
                        found_change = true;
                    }
                }
                EntryState::Invalid => {
                    warn!("Skipping output {} which has a bad signature.", i);
                }
                EntryState::Valid => { }
                EntryState::Received => {
                    if !entry.spent {
                        if found_amount < total_amount + (size_bytes * fee_rate / 1000) {
                            spend.input.push(spend::Input::from_entry(&entry));
                            size_bytes += 150; // 40 txin stuff, 72 sig, 33 key
                            found_amount += entry.amount;
                        }
                    }
                }
            }
            // Early quit if we have change and sufficient funds
            if found_change && found_amount >= total_amount + (size_bytes * fee_rate / 1000) {
                break;
            }
        }

        // Assess what we found and return errors if necessary
        let total_needed = total_amount + (size_bytes * fee_rate / 1000);
        if found_amount < total_needed {
            return Err(Error::InsufficientFunds(found_amount, total_needed));
        }
        let computed_change = found_amount - total_needed;
        if computed_change < CHANGE_DUST {
            spend.change_amount = 0;
            spend.change_path = [0; 5];
            if found_change {
                spend.output.pop();
            }
        } else {
            spend.change_amount = computed_change;
            spend.output.last_mut().unwrap().value = computed_change;
            spend.change_vout = spend.output.len() as u32 - 1;  // TODO shuffle
            if !found_change {
                return Err(Error::WalletFull);
            }
        }

        // If no errors, we're done!
        Ok(())
    }

    /// Obtain a scriptsig from the dongle for a specific input in a spending transaction
    pub fn get_script_sig<D: Dongle>(&self, dongle: &mut D, spend: &spend::Spend, index: usize, continuing: bool) -> Result<Script, Error> {
        dongle.transaction_input_start(spend, index, continuing)?;
        dongle.transaction_input_finalize(spend)?;
        let signing_pk_path = bip32_path(self.network, self.account, KeyPurpose::Address, index as u32);
        let signing_pk = dongle.get_public_key(&signing_pk_path, false)?;
        let mut vec_sig = dongle.transaction_sign(signing_pk_path, SigHashType::All, 0)?;
        vec_sig[0] = 0x30;
        Ok(script::Builder::new().push_slice(&vec_sig[..])
                                 .push_slice(&signing_pk.public_key.serialize())
                                 .into_script())
    }

    /// Accessor for the account number
    pub fn account(&self) -> u32 { self.account }
    /// Accessor for the number of entries
    pub fn n_entries(&self) -> usize { self.entries.len() }
}

/// Whether an entry has been used
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EntryState {
    /// Entry is all zeroes
    Unused,
    /// Entry is nonzero but has no associated txout
    Valid,
    /// Entry is nonzero and has an associated txout
    Received,
    /// Entry is nonzero but has an invalid signature
    Invalid
}

/// Structure representing a decrypted entry
///
/// The full byte format is
/// +------------+-----------------------------------------+-----------+--------+
/// | Field      | Description                             | Size      | Offset |
/// +------------+-----------------------------------------+-----------+--------|
/// | Signature  | Sig w address of all following fields   |  64 bytes | 0      |
/// | Trusted In | "Trusted Input" to send to dongle       |  56 bytes | 64     |
/// | Txid       | TXID of first output using this address |  32 bytes | 120    |
/// | vout       | vout of said output, big endian         |   4 bytes | 152    |
/// | Amount     | Amount of said output, big endian       |   8 bytes | 156    |
/// | Date       | ASCII bytes YYYY-MM-DD HH:MM:SS+ZZZZ    |  24 bytes | 164    |
/// | Blockhash  | Recent blockhash, big endian            |  32 bytes | 188    |
/// | User ID    | Freeform, zero-padded, expected ASCII   |  32 bytes | 220    |
/// | Note       | Freeform, zero-padded, expected ASCII   |  80 bytes | 252    |
/// | Flags      | 0 for unspent, 1 for spent              |   4 bytes | 332    |
/// +------------+-----------------------------------------+-----------+--------+
///
/// Total: 336 bytes
/// Total signed: 276 bytes
///
pub struct Entry {
    /// The overall state of this entry
    pub state: EntryState,
    /// The BIP32 path leading to this entry's address
    pub bip32_path: [u32; 5],
    /// Whether or not this output is marked as having been spent
    pub spent: bool,
    /// The "trusted input", a txid:vout:amount triple encrypted for the dongle by itself
    pub trusted_input: [u8; 56],
    /// The Bitcoin address of this entry
    pub address: Address,
    /// The BIP32 index of this entry
    pub index: usize,
    /// The txid of the first receive to this address (or all zeros if it's yet unused)
    pub txid: [u8; 32],
    /// The vout of the first receive to this address (or zero if it's yet unused)
    pub vout: u32,
    /// The amount of the first receive to this address (or zero if it's yet unused)
    pub amount: u64,
    /// The date the entry was updated, in ASCII `YYYY-MM-DD HH:MM:SS+ZZZZ`
    pub date: [u8; 24],
    /// A recent bitcoin blockhash
    pub blockhash: [u8; 32],
    /// A freeform user ID, max 32 bytes
    pub user: String,
    /// A freeform note
    pub note: String
}

impl Entry {
    /// Encode an entry, sign the second half of it, and embed the signature in the entry
    fn sign_and_encrypt<D: Dongle>(&self, dongle: &mut D, network: Network, account: u32, index: usize) -> Result<[u8; ENCRYPTED_ENTRY_SIZE], Error> {
        let mut input = [0; DECRYPTED_ENTRY_SIZE];
        // Copy out the signed data
        input[64..120].copy_from_slice(&self.trusted_input);
        input[120..152].copy_from_slice(&self.txid);
        BigEndian::write_u32(&mut input[152..156], self.vout);
        BigEndian::write_u64(&mut input[156..164], self.amount);
        input[164..188].copy_from_slice(&self.date);
        input[188..220].copy_from_slice(&self.blockhash);
        input[220..220 + self.user.as_bytes().len()].copy_from_slice(self.user.as_bytes());
        input[252..252 + self.note.as_bytes().len()].copy_from_slice(self.note.as_bytes());
        BigEndian::write_u32(&mut input[332..336], if self.spent { 1 } else { 0 });
        // Now sign it
        let sig = {
            let to_sign = &input[64..336];

            println!("The dongle will ask you to sign hash {}", hash_sha256(to_sign).to_hex());
            println!("This is the SHA256 of data {}", to_sign.to_hex());
            dongle.sign_message(to_sign, &bip32_path(network, account, KeyPurpose::Address, index as u32))?
        };
        input[0..64].copy_from_slice(&sig);

        // AES-encrypt the whole thing
        let mut ret = [0; ENCRYPTED_ENTRY_SIZE];
        encrypt(dongle, network, account, index, &input, &mut ret)?;
        Ok(ret)
    }

    /// Interpret a byte sequence as an entry; verify its signature if it's not blank
    fn decrypt_and_verify<D: Dongle>(dongle: &mut D, network: Network, account: u32, index: usize, input: &[u8; ENCRYPTED_ENTRY_SIZE]) -> Result<Entry, Error> {
        let mut data = [0u8; DECRYPTED_ENTRY_SIZE];
        decrypt(dongle, network, account, index, &input[..], &mut data)?;

        let path = bip32_path(network, account, KeyPurpose::Address, index as u32);
        let key = dongle.get_public_key(&path, false)?;
        if data[164..188].iter().all(|x| *x == 0) {  // check for zeroed out date
            Ok(Entry {
                state: EntryState::Unused,
                bip32_path: path,
                spent: false,
                trusted_input: [0; 56],
                address: Address::from_str(&key.b58_address)?,
                index: index,
                txid: [0; 32],
                vout: 0,
                amount: 0,
                date: [0; 24],
                user: String::new(),
                blockhash: [0; 32],
                note: String::new()
            })
        } else {
            let secp = Secp256k1::verification_only();
            let sig = convert_compact_to_secp(&data[0..64])?;
            let mut msg_full = vec![0; 300];
            // nb the x18 here is the length of "Bitcoin Signed Message:\n", the xfdx00x01 is the length of the rest
            msg_full[0..28].copy_from_slice(b"\x18Bitcoin Signed Message:\n\xfd\x10\x01");
            msg_full[28..300].copy_from_slice(&data[64..336]);
            let msg_hash = hash_sha256(&hash_sha256(&msg_full));
            let msg = secp256k1::Message::from_slice(&msg_hash).unwrap();
            let verified = secp.verify(&msg, &sig, &key.public_key).is_ok();

            let mut trusted_input = [0; 56]; trusted_input.clone_from_slice(&data[64..120]);
            let mut txid = [0; 32]; txid.clone_from_slice(&data[120..152]);
            let mut date = [0; 24]; date.clone_from_slice(&data[164..188]);
            let mut hash = [0; 32]; hash.clone_from_slice(&data[188..220]);

            let state;
            if verified {
                if trusted_input.iter().all(|x| *x == 0) {
                    state = EntryState::Valid;
                } else {
                    state = EntryState::Received;
                }
            } else {
                state = EntryState::Invalid;
            }

            Ok(Entry {
                state: state,
                bip32_path: path,
                spent: BigEndian::read_u32(&data[332..336]) == 1,
                trusted_input: trusted_input,
                address: Address::from_str(&key.b58_address)?,
                index: index,
                txid: txid,
                vout: BigEndian::read_u32(&data[152..156]),
                amount: BigEndian::read_u64(&data[156..164]),
                date: date,
                user: String::from_utf8(data[220..252].to_owned())?,
                blockhash: hash,
                note: String::from_utf8(data[252..332].to_owned())?
            })
        }
    }

    /// Produce a Bitcoin signed message using this entry's address
    pub fn sign_message<D: Dongle>(&self, dongle: &mut D, msg: &str) -> Result<[u8; 64], Error> {
        let msg = msg.as_bytes();
        println!("The dongle will ask you to sign hash {}", hash_sha256(msg).to_hex());
        Ok(dongle.sign_message(msg, &self.bip32_path)?)
    }
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Note that for an unused key we quit immediately. In particular it
        // is important that we not expose the address to the user until they
        // create a signed entry for it, otherwise it defeats the purpose of
        // the audit log
        match self.state {
            EntryState::Unused => return write!(f, "[unused]"),
            EntryState::Invalid => writeln!(f, "**** INVALID SIGNATURE **** :")?,
            EntryState::Valid => writeln!(f, "Signed Entry (unused):")?,
            EntryState::Received => writeln!(f, "Signed Entry (used):")?
        }
        writeln!(f, "   index: {}", self.index)?;
        writeln!(f, " address: {}", self.address)?;
        if self.state != EntryState::Received {
            writeln!(f, "    txid: no associated output")?;
        } else {
            let txid = Sha256dHash::from(&self.txid[..]);
            writeln!(f, "    txid: {}", txid)?;
            writeln!(f, "    vout: {}", self.vout)?;
            writeln!(f, "  amount: {}", self.amount)?;
            writeln!(f, "   spent: {}", self.spent)?;
        }
        writeln!(f, " created: {}", str::from_utf8(&self.date[..]).unwrap())?;
        let blockhash = Sha256dHash::from(&self.blockhash[..]);
        writeln!(f, " (after): {}", blockhash)?;
        writeln!(f, "    user: {}", self.user)?;
        write!(f, "    note: {}", self.note)?;
        Ok(())
    }
}

