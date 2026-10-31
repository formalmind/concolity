//! Naming storage slots with solc's `storageLayout`.
//!
//! A slot is either a state variable's own (`total` at slot 1), somewhere inside one (`a.b` of a struct, `xs[2]` of
//! a fixed array), or derived by hashing: `mapping[k]` lives at `keccak(k . p)` and a dynamic array's data at
//! `keccak(p)`, where `p` is the variable's slot. Hashes don't run backwards, so the trace records every KECCAK256
//! input (the preimages) and a slot is named by finding the hash it is an offset from.

use revm::primitives::{keccak256, Address, B256, I256, U256};
use serde_json::Value;
use std::collections::HashMap;

/// A named place in storage: what part of a slot holds which variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// `accounts[0x…].shares`, `history[0]`, `history.length`.
    pub path: String,
    /// The storage type id (`t_uint256`), a key into the layout's `types`.
    pub ty: String,
    /// Byte offset of the value inside the slot (packed variables share a slot).
    pub offset: usize,
}

pub struct Layout {
    vars: Vec<(String, U256, usize, String)>,
    types: HashMap<String, Value>,
}

/// keccak256 output → input, from the trace.
pub type Preimages = HashMap<B256, Vec<u8>>;

/// How far past a hash a slot can be and still count as an offset from it (struct members, array elements).
const MAX_OFFSET: u64 = 1 << 24;

impl Layout {
    pub fn new(layout: &Value) -> Self {
        let vars = layout["storage"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| {
                (
                    v["label"].as_str().unwrap_or("?").to_string(),
                    v["slot"].as_str().and_then(|s| s.parse::<U256>().ok()).unwrap_or_default(),
                    v["offset"].as_u64().unwrap_or(0) as usize,
                    v["type"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        let types = layout["types"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        Layout { vars, types }
    }

    fn ty(&self, id: &str) -> &Value {
        self.types.get(id).unwrap_or(&Value::Null)
    }

    /// How many bytes a type takes in storage.
    pub fn size(&self, id: &str) -> usize {
        self.bytes(id)
    }

    fn bytes(&self, id: &str) -> usize {
        self.ty(id)["numberOfBytes"].as_str().and_then(|s| s.parse().ok()).unwrap_or(32)
    }

    fn slots(&self, id: &str) -> U256 {
        U256::from(self.bytes(id).div_ceil(32).max(1))
    }

    pub fn label(&self, id: &str) -> String {
        self.ty(id)["label"].as_str().unwrap_or(id).to_string()
    }

    /// A root variable by name: its slot, offset and type (for setting up state before a run).
    pub fn var(&self, name: &str) -> Option<(U256, usize, String)> {
        self.vars.iter().find(|v| v.0 == name).map(|v| (v.1, v.2, v.3.clone()))
    }

    /// Everything that lives in `slot`, outermost first. Values only (mappings and structs are containers; a dynamic
    /// array's own slot is its length).
    pub fn places(&self, slot: U256, pre: &Preimages) -> Vec<Place> {
        self.at(slot, pre, 0).into_iter().filter(|p| self.is_value(&p.ty)).map(|mut p| {
            if self.ty(&p.ty)["encoding"] == "dynamic_array" {
                p.path.push_str(".length");
            }
            p
        }).collect()
    }

    fn is_value(&self, id: &str) -> bool {
        let t = self.ty(id);
        t["encoding"] == "dynamic_array" || (t["encoding"] == "inplace" && t["members"].is_null() && t["base"].is_null())
            || t["encoding"] == "bytes"
    }

    /// Every place (containers too) whose storage starts in `slot`.
    fn at(&self, slot: U256, pre: &Preimages, depth: usize) -> Vec<Place> {
        let mut out = Vec::new();
        for (label, start, offset, ty) in &self.vars {
            if *start == slot {
                self.expand(label.clone(), ty, *offset, &mut out);
            } else if *start < slot && slot < *start + self.slots(ty) {
                self.within(label.clone(), ty, slot - *start, &mut out);
            }
        }
        if depth > 6 {
            return out;
        }
        for (hash, input) in pre {
            let h = U256::from_be_bytes(hash.0);
            if h > slot || slot - h >= U256::from(MAX_OFFSET) {
                continue;
            }
            let delta = slot - h;
            // mapping[key] lives at keccak(key . slot): the last 32 bytes are the mapping's slot.
            if input.len() >= 32 {
                let (key, base) = input.split_at(input.len() - 32);
                let base = U256::from_be_slice(base);
                for p in self.at(base, pre, depth + 1) {
                    let t = self.ty(&p.ty);
                    if t["encoding"] == "mapping" && p.offset == 0 {
                        let key_ty = t["key"].as_str().unwrap_or("");
                        let value_ty = t["value"].as_str().unwrap_or("").to_string();
                        let path = format!("{}[{}]", p.path, self.format_key(key_ty, key));
                        self.within(path, &value_ty, delta, &mut out);
                    }
                }
            }
            // A dynamic array's data starts at keccak(slot).
            if input.len() == 32 {
                for p in self.at(U256::from_be_slice(input), pre, depth + 1) {
                    let t = self.ty(&p.ty);
                    if t["encoding"] == "dynamic_array" && p.offset == 0 {
                        let base = t["base"].as_str().unwrap_or("").to_string();
                        let per = self.bytes(&base);
                        if per >= 32 {
                            let n = self.slots(&base);
                            let index = delta / n;
                            self.within(format!("{}[{index}]", p.path), &base, delta % n, &mut out);
                        } else {
                            // Packed elements: several per slot.
                            let k = U256::from(32 / per);
                            for j in 0..32 / per {
                                let index = delta * k + U256::from(j);
                                out.push(Place { path: format!("{}[{index}]", p.path), ty: base.clone(), offset: j * per });
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// `delta` slots into a value of type `ty` called `path`.
    fn within(&self, path: String, ty: &str, delta: U256, out: &mut Vec<Place>) {
        if delta.is_zero() {
            return self.expand(path, ty, 0, out);
        }
        let t = self.ty(ty);
        if let Some(members) = t["members"].as_array() {
            for m in members {
                let start = m["slot"].as_str().and_then(|s| s.parse::<U256>().ok()).unwrap_or_default();
                let mty = m["type"].as_str().unwrap_or("");
                if start <= delta && delta < start + self.slots(mty) {
                    let name = format!("{path}.{}", m["label"].as_str().unwrap_or("?"));
                    if start == delta {
                        self.expand(name, mty, m["offset"].as_u64().unwrap_or(0) as usize, out);
                    } else {
                        self.within(name, mty, delta - start, out);
                    }
                }
            }
        } else if t["encoding"] == "inplace" && t["base"].is_string() {
            // A fixed-size array: elements one after another, packed when small.
            let base = t["base"].as_str().unwrap_or("");
            let per = self.bytes(base);
            if per >= 32 {
                let n = self.slots(base);
                self.within(format!("{path}[{}]", delta / n), base, delta % n, out);
            } else {
                for j in 0..32 / per {
                    out.push(Place { path: format!("{path}[{}]", delta * U256::from(32 / per) + U256::from(j)), ty: base.into(), offset: j * per });
                }
            }
        }
    }

    /// A place that starts at a slot boundary, and whatever starts there with it (a struct's first members).
    fn expand(&self, path: String, ty: &str, offset: usize, out: &mut Vec<Place>) {
        out.push(Place { path: path.clone(), ty: ty.to_string(), offset });
        let t = self.ty(ty);
        if let Some(members) = t["members"].as_array() {
            for m in members.iter().filter(|m| m["slot"] == "0") {
                let name = format!("{path}.{}", m["label"].as_str().unwrap_or("?"));
                self.expand(name, m["type"].as_str().unwrap_or(""), m["offset"].as_u64().unwrap_or(0) as usize, out);
            }
        } else if t["encoding"] == "inplace" && t["base"].is_string() {
            let base = t["base"].as_str().unwrap_or("");
            let per = self.bytes(base);
            let n = if per >= 32 { 1 } else { 32 / per };
            for j in 0..n {
                self.expand(format!("{path}[{j}]"), base, j * per.min(32), out);
            }
        }
    }

    fn format_key(&self, key_ty: &str, key: &[u8]) -> String {
        let label = self.label(key_ty);
        if key.len() == 32 {
            format_word(&label, U256::from_be_slice(key), 32)
        } else {
            // string / bytes keys hash their contents.
            String::from_utf8(key.to_vec()).map(|s| format!("{s:?}")).unwrap_or_else(|_| format!("0x{}", revm::primitives::hex::encode(key)))
        }
    }

    /// The value a place holds in a whole slot word.
    pub fn decode(&self, place: &Place, word: U256) -> String {
        let n = self.bytes(&place.ty).min(32);
        let v = if n >= 32 { word } else { (word >> (place.offset * 8)) & ((U256::from(1) << (n * 8)) - U256::from(1)) };
        if self.ty(&place.ty)["encoding"] == "dynamic_array" {
            return v.to_string();
        }
        format_word(&self.label(&place.ty), v, n)
    }
}

/// A value of an elementary type, as Solidity would write it.
pub fn format_word(label: &str, v: U256, bytes: usize) -> String {
    if label == "bool" {
        return (!v.is_zero()).to_string();
    }
    if label == "address" || label == "address payable" || label.starts_with("contract ") {
        return Address::from_word(B256::from(v)).to_checksum(None);
    }
    if let Some(bits) = label.strip_prefix("int").and_then(|b| b.parse::<usize>().ok()) {
        // Sign-extend from `bits`.
        let shift = 256 - bits;
        let raw = I256::from_raw(v << shift).asr(shift);
        return raw.to_string();
    }
    if label.starts_with("bytes") {
        let b = B256::from(v << ((32 - bytes) * 8));
        return format!("0x{}", revm::primitives::hex::encode(&b[..bytes]));
    }
    if label.starts_with("enum ") || label.starts_with("uint") {
        return v.to_string();
    }
    format!("{v:#x}")
}

/// keccak256 of a slot, for tests and setup: where a dynamic array's data starts.
pub fn array_data(slot: U256) -> U256 {
    U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0)
}

/// keccak256(key . slot) for a 32-byte key: a mapping value's slot.
pub fn mapping_slot(key: B256, slot: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&key.0);
    buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}
