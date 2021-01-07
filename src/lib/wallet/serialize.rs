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

//! Wallet Serialization
//!
//! Data types which can be read and written to the wallet backing store
//!

use miniscript::bitcoin::{self, hashes::Hash, util::bip32};
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::str::FromStr;

// Largest size of a script we will serialize
const MAX_SCRIPTPUBKEY_SIZE: u32 = 50;
// Largest number of elements in any vector we will serialize
const MAX_VEC_ELEMS: u32 = 10_000;
// Largest size of a user-provided note string
const MAX_STRING_LEN: u32 = 100_000;
// Largest size of an individual descirptor string
const MAX_DESCRIPTOR_LEN: u32 = 64 * 1024;

/// Trait describing an object which can be de/serialized to the wallet storage
pub trait Serialize: Sized {
    /// Write the data to a writer
    fn write_to<W: Write>(&self, w: W) -> io::Result<()>;

    /// Read the data from a reader
    fn read_from<R: Read>(r: R) -> io::Result<Self>;
}

impl Serialize for u8 {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        w.write_all(&[*self])
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let mut dat = [0; 1];
        r.read_exact(&mut dat)?;
        Ok(dat[0])
    }
}

impl Serialize for u32 {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        w.write_all(&[
            *self as u8,
            (*self >> 8) as u8,
            (*self >> 16) as u8,
            (*self >> 24) as u8,
        ])
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let mut dat = [0; 4];
        r.read_exact(&mut dat)?;
        Ok((dat[0] as u32) + ((dat[1] as u32) << 8) + ((dat[2] as u32) << 16) + ((dat[3] as u32) << 24))
    }
}

impl Serialize for u64 {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        (*self as u32).write_to(&mut w)?;
        ((*self >> 32) as u32).write_to(w)
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let lo: u32 = Serialize::read_from(&mut r)?;
        let hi: u32 = Serialize::read_from(r)?;
        Ok((lo as u64) + ((hi as u64) << 32))
    }
}

impl Serialize for miniscript::bitcoin::Txid {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        w.write_all(&self[..])
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let mut dat = [0; 32];
        r.read_exact(&mut dat)?;
        Ok(miniscript::bitcoin::Txid::from_inner(dat))
    }
}

impl Serialize for miniscript::bitcoin::OutPoint {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        self.txid.write_to(&mut w)?;
        self.vout.write_to(w)
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        Ok(miniscript::bitcoin::OutPoint {
            txid: Serialize::read_from(&mut r)?,
            vout: Serialize::read_from(r)?,
        })
    }
}

impl<T: Serialize> Serialize for Vec<T> {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        let len32: u32 = self.len() as u32;
        if self.len() > MAX_VEC_ELEMS as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "writing vector of length {} exceeded max {} (type {})",
                    len32,
                    MAX_VEC_ELEMS,
                    std::any::type_name::<Self>(),
                ),
            ));
        }
        len32.write_to(&mut w)?;
        for t in self {
            t.write_to(&mut w)?;
        }
        Ok(())
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let len32: u32 = Serialize::read_from(&mut r)?;
        let mut ret = Vec::with_capacity(len32 as usize);
        if len32 > MAX_VEC_ELEMS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reading vector of length {} exceeded max {} (type {})",
                    len32,
                    MAX_VEC_ELEMS,
                    std::any::type_name::<Self>(),
                ),
            ));
        }

        for _ in 0..len32 {
            ret.push(Serialize::read_from(&mut r)?);
        }
        Ok(ret)
    }
}

impl<T: Eq + std::hash::Hash + Serialize> Serialize for HashSet<T> {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        let len32: u32 = self.len() as u32;
        if self.len() > MAX_VEC_ELEMS as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "writing set of length {} exceeded max {} (type {})",
                    len32,
                    MAX_VEC_ELEMS,
                    std::any::type_name::<Self>(),
                ),
            ));
        }
        len32.write_to(&mut w)?;
        for t in self {
            t.write_to(&mut w)?;
        }
        Ok(())
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let len32: u32 = Serialize::read_from(&mut r)?;
        let mut ret = HashSet::with_capacity(len32 as usize);
        if len32 > MAX_VEC_ELEMS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reading set of length {} exceeded max {} (type {})",
                    len32,
                    MAX_VEC_ELEMS,
                    std::any::type_name::<Self>(),
                ),
            ));
        }

        for _ in 0..len32 {
            ret.insert(Serialize::read_from(&mut r)?);
        }
        Ok(ret)
    }
}

impl Serialize for String {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        let len32: u32 = self.len() as u32;
        if self.len() > MAX_STRING_LEN as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "writing string of length {} exceeded max {} (type {})",
                    len32,
                    MAX_STRING_LEN,
                    std::any::type_name::<Self>(),
                ),
            ));
        }
        len32.write_to(&mut w)?;
        w.write_all(self.as_bytes())
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let len32: u32 = Serialize::read_from(&mut r)?;
        let mut ret = vec![0; len32 as usize];
        if len32 > MAX_STRING_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reading string of length {} exceeded max {} (type {})",
                    len32,
                    MAX_STRING_LEN,
                    std::any::type_name::<Self>(),
                ),
            ));
        }

        r.read_exact(&mut ret[..])?;
        String::from_utf8(ret).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl<T: Eq + std::hash::Hash + Serialize, V: Serialize> Serialize for HashMap<T, V> {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        let len32: u32 = self.len() as u32;
        if self.len() > MAX_VEC_ELEMS as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "writing map of length {} exceeded max {} (type {})",
                    len32,
                    MAX_VEC_ELEMS,
                    std::any::type_name::<Self>(),
                ),
            ));
        }
        len32.write_to(&mut w)?;
        for (t, v) in self {
            t.write_to(&mut w)?;
            v.write_to(&mut w)?;
        }
        Ok(())
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let len32: u32 = Serialize::read_from(&mut r)?;
        let mut ret = HashMap::with_capacity(len32 as usize);
        if len32 > MAX_VEC_ELEMS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reading map of length {} exceeded max {} (type {})",
                    len32,
                    MAX_VEC_ELEMS,
                    std::any::type_name::<Self>(),
                ),
            ));
        }

        for _ in 0..len32 {
            ret.insert(
                Serialize::read_from(&mut r)?,
                Serialize::read_from(&mut r)?,
            );
        }
        Ok(ret)
    }
}

// bitcoin types

impl Serialize for bitcoin::PublicKey {
    fn write_to<W: Write>(&self, w: W) -> io::Result<()> {
        // FIXME this may panic, pending new rust-bitcoin release for fix..
        Ok(self.write_into(w))
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        // FIXME copied from https://github.com/rust-bitcoin/rust-bitcoin/pull/542 inline this when that is merged
        let mut bytes = [0; 65];
        let byte_sl;
        r.read_exact(&mut bytes[0..1])?;
        if bytes[0] < 4 {
            r.read_exact(&mut bytes[1..33])?;
            byte_sl = &bytes[0..33];
        } else {
            r.read_exact(&mut bytes[1..65])?;
            byte_sl = &bytes[0..65];
        }
        Self::from_slice(byte_sl).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl Serialize for bip32::DerivationPath {
    fn write_to<W: Write>(&self, w: W) -> io::Result<()> {
        // We could avoid this allocation if we were less lazy..
        let sl: &[bip32::ChildNumber] = &self.as_ref();
        let vec: Vec<u32> = sl.iter().cloned().map(From::from).collect();
        vec.write_to(w)
    }

    fn read_from<R: Read>(r: R) -> io::Result<Self> {
        let path: Vec<u32> = Serialize::read_from(r)?;
        let vec: Vec<bip32::ChildNumber> = path.into_iter().map(From::from).collect();
        Ok(bip32::DerivationPath::from(vec))
    }
}

impl Serialize for miniscript::Descriptor<miniscript::DescriptorPublicKey> {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        let string = self.to_string();
        (string.len() as u32).write_to(&mut w)?;
        w.write_all(string.as_bytes())
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let len: u32 = Serialize::read_from(&mut r)?;
        if len > MAX_DESCRIPTOR_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reading descriptor of length {} exceeded max {}",
                    len,
                    MAX_DESCRIPTOR_LEN,
                ),
            ));
        }
        let mut data = vec![0; len as usize];
        r.read_exact(&mut data)?;
        let s = String::from_utf8(data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        miniscript::Descriptor::from_str(&s)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }
}

impl Serialize for bitcoin::Script {
    fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        let len32: u32 = self.len() as u32;
        if self.len() > MAX_SCRIPTPUBKEY_SIZE as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "writing script of length {} exceeded max {} (type {})",
                    len32,
                    MAX_SCRIPTPUBKEY_SIZE,
                    std::any::type_name::<Self>(),
                ),
            ));
        }
        len32.write_to(&mut w)?;
        w.write_all(self.as_bytes())
    }

    fn read_from<R: Read>(mut r: R) -> io::Result<Self> {
        let len32: u32 = Serialize::read_from(&mut r)?;
        let mut ret = vec![0; len32 as usize];
        if len32 > MAX_SCRIPTPUBKEY_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reading script of length {} exceeded max {} (type {})",
                    len32,
                    MAX_SCRIPTPUBKEY_SIZE,
                    std::any::type_name::<Self>(),
                ),
            ));
        }

        r.read_exact(&mut ret[..])?;
        Ok(bitcoin::Script::from(ret))
    }
}


#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use miniscript::bitcoin::OutPoint;

    use super::*;

    #[test]
    fn basic_rtt() {
        let data = vec![
            OutPoint::from_str("2222222222222222222222222222222222222222222222222222222222222222:0").unwrap(),
            OutPoint::from_str("3322222222222222222222222222222222222222222222222222222222222222:1").unwrap(),
            OutPoint::from_str("abcdabcda2923183201028930893081903819023810982301928301232222222:0").unwrap(),
            OutPoint::from_str("2222222220132823173987123973219789379379122222222222222222222222:9999").unwrap(),
        ];

        let mut ser = vec![];
        data.write_to(&mut ser).expect("writing");
        let read: Vec<OutPoint> = Serialize::read_from(&ser[..]).expect("read");
        assert_eq!(data, read);
    }
}
