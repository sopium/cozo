use rmp_serde::Serializer;
use serde::Serialize;

use crate::data::value::DataValue;

#[derive(Debug, thiserror::Error)]
pub enum TupleError {
    #[error("bad data: {0} for {1:x?}")]
    BadData(String, Vec<u8>),
}

pub(crate) struct Tuple(Vec<DataValue>);

impl Tuple {
    pub(crate) fn arity(&self) -> usize {
        self.0.len()
    }
    pub(crate) fn encode_as_key(&self, prefix: u32) -> Vec<u8> {
        let len = self.arity();
        let mut ret = Vec::with_capacity(4 + 4 * len + 10 * len);
        ret.extend(prefix.to_be_bytes());
        ret.extend((len as u32).to_be_bytes());
        ret.resize(4 * (len + 1), 0);
        for (idx, val) in self.0.iter().enumerate() {
            if idx > 0 {
                let pos = (ret.len() as u32).to_be_bytes();
                for (i, u) in pos.iter().enumerate() {
                    ret[4 * (1 + idx) + i] = *u;
                }
            }
            val.serialize(&mut Serializer::new(&mut ret)).unwrap();
        }
        ret
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct EncodedTuple<'a>(&'a [u8]);

impl<'a> From<&'a [u8]> for EncodedTuple<'a> {
    fn from(s: &'a [u8]) -> Self {
        EncodedTuple(s)
    }
}

impl<'a> EncodedTuple<'a> {
    pub(crate) fn prefix(&self) -> Result<u32, TupleError> {
        if self.0.len() < 4 {
            Err(TupleError::BadData(
                "bad data length".to_string(),
                self.0.to_vec(),
            ))
        } else {
            Ok(u32::from_be_bytes([
                self.0[0], self.0[1], self.0[2], self.0[3],
            ]))
        }
    }
    pub(crate) fn arity(&self) -> Result<usize, TupleError> {
        if self.0.len() < 8 {
            Err(TupleError::BadData(
                "bad data length".to_string(),
                self.0.to_vec(),
            ))
        } else {
            Ok(u32::from_be_bytes([self.0[4], self.0[5], self.0[6], self.0[7]]) as usize)
        }
    }
    pub(crate) fn get(&self, idx: usize) -> anyhow::Result<DataValue> {
        let pos = if idx == 0 {
            4 * (self.arity()? + 1)
        } else {
            let len_pos = (idx + 1) * 4;
            if self.0.len() < len_pos + 4 {
                return Err(
                    TupleError::BadData("bad data length".to_string(), self.0.to_vec()).into(),
                );
            }
            u32::from_be_bytes([
                self.0[len_pos],
                self.0[len_pos + 1],
                self.0[len_pos + 2],
                self.0[len_pos + 3],
            ]) as usize
        };
        if pos >= self.0.len() {
            return Err(TupleError::BadData("bad data length".to_string(), self.0.to_vec()).into());
        }
        Ok(rmp_serde::from_slice(&self.0[pos..])?)
    }

    pub(crate) fn iter(&self) -> EncodedTupleIter<'a> {
        EncodedTupleIter {
            tuple: *self,
            size: 0,
            pos: 0,
        }
    }
}

struct EncodedTupleIter<'a> {
    tuple: EncodedTuple<'a>,
    size: usize,
    pos: usize,
}

impl<'a> Iterator for EncodedTupleIter<'a> {
    type Item = anyhow::Result<DataValue>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.size == 0 {
            let arity = match self.tuple.arity() {
                Ok(a) => a,
                Err(e) => return Some(Err(e.into())),
            };
            self.size = arity;
        }
        if self.pos == self.size {
            None
        } else {
            let pos = self.pos;
            self.pos += 1;
            Some(self.tuple.get(pos))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::data::tuple::{EncodedTuple, Tuple};
    use crate::data::value::DataValue;

    #[test]
    fn test_serde() {
        let val: Vec<DataValue> = vec![
            json!(1).into(),
            json!(2.0).into(),
            json!("my_name_is").into(),
        ];
        let val = Tuple(val);
        let encoded = val.encode_as_key(123);
        println!("{:x?}", encoded);
        let encoded_tuple: EncodedTuple = (&encoded as &[u8]).into();
        println!("{:?}", encoded_tuple.prefix());
        println!("{:?}", encoded_tuple.arity());
        println!("{:?}", encoded_tuple.get(0));
        println!("{:?}", encoded_tuple.get(1));
        println!("{:?}", encoded_tuple.get(2));
        println!("{:?}", encoded_tuple.get(3));
        println!(
            "{:?}",
            encoded_tuple
                .iter()
                .collect::<anyhow::Result<Vec<DataValue>>>()
        )
    }
}
