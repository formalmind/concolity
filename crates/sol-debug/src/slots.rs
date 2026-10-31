//! Naming storage slots from solc's storage layout, for state diffs: a slot becomes `values[1]`, `matrix[1][2]`,
//! `config.owner`, `balances[0x1234…]` or `name[2]` (a long string's data), with its type and decoded values. A
//! port of foundry's `slot_identifier.rs` and `mapping_slots.rs` (v1.8.6), so the output matches forge's.

use alloy_dyn_abi::{DynSolType, DynSolValue};
use revm::primitives::{hex, keccak256, B256, U256};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

/// Mapping slots seen while recording: a slot written through `keccak256(key . parent)` knows its key and parent.
#[derive(Debug, Clone, Default)]
pub struct MappingSlots {
    pub parent_slots: HashMap<B256, B256>,
    pub keys: HashMap<B256, B256>,
    seen_sha3: HashMap<B256, (B256, B256)>,
}

impl MappingSlots {
    /// A KECCAK256 over 64 bytes of memory: maybe a mapping slot (`key . parent`).
    pub fn record_hash(&mut self, data: &[u8]) {
        if data.len() != 64 {
            return;
        }
        let result = keccak256(data);
        self.seen_sha3.insert(result, (B256::from_slice(&data[..32]), B256::from_slice(&data[32..])));
    }

    /// An SSTORE: if its slot is a hash seen above, it (and its parents) are mapping entries.
    pub fn insert(&mut self, slot: B256) -> bool {
        match self.seen_sha3.get(&slot).copied() {
            Some((key, parent)) => {
                if self.keys.insert(slot, key).is_some() {
                    return false;
                }
                self.parent_slots.insert(slot, parent);
                self.insert(parent);
                true
            }
            None => false,
        }
    }
}

/// A token as forge prints it (`format_token_raw`).
pub fn format_raw(v: &DynSolValue) -> String {
    match v {
        DynSolValue::Uint(u, _) => u.to_string(),
        DynSolValue::Int(i, _) => i.to_string(),
        DynSolValue::Address(a) => a.to_checksum(None),
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::String(s) => s.clone(),
        DynSolValue::Bytes(b) => hex::encode_prefixed(b),
        DynSolValue::FixedBytes(b, n) => hex::encode_prefixed(&b[..*n]),
        DynSolValue::Array(v) | DynSolValue::FixedArray(v) => format!("[{}]", v.iter().map(format_raw).collect::<Vec<_>>().join(", ")),
        DynSolValue::Tuple(v) => format!("({})", v.iter().map(format_raw).collect::<Vec<_>>().join(", ")),
        DynSolValue::CustomStruct { tuple, .. } => format!("({})", tuple.iter().map(format_raw).collect::<Vec<_>>().join(", ")),
        other => hex::encode_prefixed(other.abi_encode()),
    }
}

#[derive(Debug, Clone)]
pub struct Decoded {
    pub previous: DynSolValue,
    pub new: DynSolValue,
}

#[derive(Debug, Clone)]
pub struct SlotInfo {
    pub label: String,
    /// The layout's type label (`uint256[3]`, `mapping(address => uint256)`).
    pub type_label: String,
    pub ty: DynSolType,
    pub offset: i64,
    pub slot: String,
    pub members: Option<Vec<SlotInfo>>,
    pub decoded: Option<Decoded>,
    pub keys: Option<Vec<String>>,
}

impl SlotInfo {
    /// The JSON fields forge flattens into a slot's diff, in its order.
    pub fn json_fields(&self, out: &mut Map<String, Value>) {
        out.insert("label".into(), json!(self.label));
        out.insert("type".into(), json!(self.type_label));
        out.insert("offset".into(), json!(self.offset));
        out.insert("slot".into(), json!(self.slot));
        if let Some(m) = &self.members {
            out.insert("members".into(), Value::Array(m.iter().map(|x| {
                let mut o = Map::new();
                x.json_fields(&mut o);
                Value::Object(o)
            }).collect()));
        }
        if let Some(d) = &self.decoded {
            out.insert("decoded".into(), json!({ "previousValue": format_raw(&d.previous), "newValue": format_raw(&d.new) }));
        }
        if let Some(k) = &self.keys {
            match k.len() {
                1 => {
                    out.insert("key".into(), json!(k[0]));
                }
                0 => {}
                _ => {
                    out.insert("keys".into(), json!(k));
                }
            }
        }
    }

    fn base_type(&self) -> &DynSolType {
        let mut t = &self.ty;
        while let DynSolType::FixedArray(e, _) = t {
            t = e;
        }
        t
    }

    pub fn is_bytes_or_string(&self) -> bool {
        matches!(self.ty, DynSolType::Bytes | DynSolType::String)
    }

    /// One word as this slot's type (short bytes and strings inline; long ones need the data slots).
    pub fn decode(&self, value: B256) -> Option<DynSolValue> {
        let t = self.base_type();
        match t {
            DynSolType::Bytes | DynSolType::String => {
                let len_byte = value.0[31];
                if len_byte & 1 == 0 {
                    let n = ((len_byte >> 1) as usize).min(31);
                    let data = value.0[..n].to_vec();
                    Some(if matches!(t, DynSolType::String) { DynSolValue::String(String::from_utf8(data).unwrap_or_default()) } else { DynSolValue::Bytes(data) })
                } else {
                    None
                }
            }
            _ => t.abi_decode(&value.0).ok(),
        }
    }

    pub fn decode_values(&mut self, previous: B256, new: B256) {
        if let Some(members) = &mut self.members {
            for m in members.iter_mut() {
                let offset = m.offset as usize;
                let size = match &m.ty {
                    DynSolType::Uint(b) | DynSolType::Int(b) => b / 8,
                    DynSolType::Address => 20,
                    DynSolType::Bool => 1,
                    DynSolType::FixedBytes(n) => *n,
                    _ => 32,
                };
                let (mut p, mut n) = ([0u8; 32], [0u8; 32]);
                if offset + size <= 32 {
                    let start = 32 - offset - size;
                    p[32 - size..].copy_from_slice(&previous.0[start..start + size]);
                    n[32 - size..].copy_from_slice(&new.0[start..start + size]);
                }
                if let (Ok(a), Ok(b)) = (m.ty.abi_decode(&p), m.ty.abi_decode(&n)) {
                    m.decoded = Some(Decoded { previous: a, new: b });
                }
            }
        } else if let (Some(a), Some(b)) = (self.decode(previous), self.decode(new)) {
            self.decoded = Some(Decoded { previous: a, new: b });
        }
    }

    /// A long bytes/string: the data slots from the base slot's length, collected into one value.
    fn aggregate(&mut self, base: &B256, values: &HashMap<B256, B256>) -> Option<Vec<u8>> {
        let base_value = values.get(base)?;
        if base_value.0[31] & 1 != 1 {
            return None;
        }
        let len = usize::try_from(U256::from_be_bytes(base_value.0) >> 1).unwrap_or(usize::MAX);
        let slots = len.div_ceil(32).min(256);
        let start = U256::from_be_bytes(keccak256(base.0).0);
        let mut members = vec![];
        let mut data = Vec::with_capacity(slots * 32);
        for i in 0..slots {
            let s = start + U256::from(i);
            members.push(SlotInfo {
                label: format!("{}[{i}]", self.label),
                type_label: self.type_label.clone(),
                ty: DynSolType::FixedBytes(32),
                offset: 0,
                slot: s.to_string(),
                members: None,
                decoded: None,
                keys: None,
            });
            if let Some(v) = values.get(&B256::from(s)) {
                let take = 32.min(len.saturating_sub(data.len()));
                data.extend_from_slice(&v.0[..take]);
            }
        }
        if !members.is_empty() {
            self.members = Some(members);
        }
        Some(data)
    }

    pub fn decode_bytes_or_string_values(&mut self, base: &B256, changes: &BTreeMap<B256, (B256, B256)>) {
        if !self.is_bytes_or_string() {
            return;
        }
        let Some(&(prev_base, new_base)) = changes.get(base) else { return };
        let mut decode = |value: B256, new: bool| -> Option<DynSolValue> {
            if value.0[31] & 1 == 1 {
                let map: HashMap<B256, B256> = changes.iter().map(|(s, (p, n))| (*s, if new { *n } else { *p })).collect();
                let data = self.aggregate(base, &map)?;
                Some(if matches!(self.ty, DynSolType::String) { DynSolValue::String(String::from_utf8(data).unwrap_or_default()) } else { DynSolValue::Bytes(data) })
            } else {
                self.decode(value)
            }
        };
        let (p, n) = (decode(prev_base, false), decode(new_base, true));
        if let (Some(p), Some(n)) = (p, n) {
            self.decoded = Some(Decoded { previous: p, new: n });
        }
    }
}

/// A Solidity type label as a decodable type: contracts are addresses, enums uint8.
fn parse_type(label: &str) -> Option<DynSolType> {
    let scalar = if label.starts_with("contract ") {
        "address"
    } else if label.starts_with("enum ") {
        "uint8"
    } else {
        return DynSolType::parse(label).ok();
    };
    let suffix = label.find('[').map_or("", |i| &label[i..]);
    DynSolType::parse(&format!("{scalar}{suffix}")).ok()
}

fn is_struct(label: &str) -> bool {
    label.starts_with("struct ")
}

fn array_base_indices(t: &DynSolType) -> String {
    match t {
        DynSolType::FixedArray(inner, _) if matches!(inner.as_ref(), DynSolType::FixedArray(..)) => format!("[0]{}", array_base_indices(inner)),
        DynSolType::FixedArray(..) => "[0]".into(),
        _ => String::new(),
    }
}

/// solc's `storageLayout` output for one contract.
pub struct Identifier<'a> {
    storage: Vec<&'a Value>,
    types: &'a Map<String, Value>,
}

fn s<'b>(v: &'b Value, k: &str) -> &'b str {
    v[k].as_str().unwrap_or("")
}

impl<'a> Identifier<'a> {
    pub fn new(layout: &'a Value) -> Option<Identifier<'a>> {
        Some(Identifier { storage: layout["storage"].as_array()?.iter().collect(), types: layout["types"].as_object()? })
    }

    fn ty(&self, id: &str) -> Option<&'a Value> {
        self.types.get(id)
    }

    fn info(label: String, type_label: &str, ty: DynSolType, offset: i64, slot: &str) -> SlotInfo {
        SlotInfo { label, type_label: type_label.into(), ty, offset, slot: slot.into(), members: None, decoded: None, keys: None }
    }

    pub fn identify(&self, slot: &B256, mapping: Option<&MappingSlots>) -> Option<SlotInfo> {
        let slot_u = U256::from_be_bytes(slot.0);
        let slot_str = slot_u.to_string();
        for st in &self.storage {
            let ty = self.ty(s(st, "type"))?;
            let ty_label = s(ty, "label");
            let parsed = parse_type(ty_label);
            let offset = st["offset"].as_i64().unwrap_or(0);
            if s(st, "slot") == slot_str {
                if let Some(p) = parsed.clone() {
                    let label = if matches!(p, DynSolType::FixedArray(..)) { format!("{}{}", s(st, "label"), array_base_indices(&p)) } else { s(st, "label").to_string() };
                    return Some(Self::info(label, ty_label, p, offset, s(st, "slot")));
                }
            }
            match s(ty, "encoding") {
                "inplace" => {
                    let start: U256 = s(st, "slot").parse().ok()?;
                    if let Some(p @ DynSolType::FixedArray(..)) = &parsed {
                        if let Some(i) = self.array_slot(st, ty, p, slot_u, start, &slot_str) {
                            return Some(i);
                        }
                    }
                    if is_struct(ty_label) {
                        if let Some(i) = self.struct_slot(s(st, "label"), ty, slot_u, start, offset, &slot_str, 0) {
                            return Some(i);
                        }
                    }
                }
                "mapping" => {
                    if let Some(m) = mapping {
                        if let Some(i) = self.mapping_slot(st, ty, slot, &slot_str, m) {
                            return Some(i);
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    pub fn identify_bytes_or_string(&self, slot: &B256, values: &HashMap<B256, B256>) -> Option<SlotInfo> {
        let slot_u = U256::from_be_bytes(slot.0);
        let slot_str = slot_u.to_string();
        for st in &self.storage {
            let Some(ty) = self.ty(s(st, "type")) else { continue };
            if s(ty, "encoding") != "bytes" {
                continue;
            }
            let Ok(base) = s(st, "slot").parse::<U256>() else { continue };
            let Some(base_value) = values.get(&B256::from(base)) else { continue };
            let ty_label = s(ty, "label");
            if slot_u == base {
                let t = match ty_label {
                    "string" => DynSolType::String,
                    "bytes" => DynSolType::Bytes,
                    _ => continue,
                };
                return Some(Self::info(s(st, "label").into(), ty_label, t, st["offset"].as_i64().unwrap_or(0), &slot_str));
            }
            if base_value.0[31] & 1 == 1 {
                let start = U256::from_be_bytes(keccak256(base.to_be_bytes::<32>()).0);
                let len = U256::from_be_bytes(base_value.0) >> 1;
                let n = (len + U256::from(31)) / U256::from(32);
                if slot_u >= start && slot_u < start + n {
                    let i: usize = (slot_u - start).try_into().unwrap_or(usize::MAX);
                    return Some(Self::info(format!("{}[{i}]", s(st, "label")), ty_label, DynSolType::FixedBytes(32), 0, &slot_str));
                }
            }
        }
        None
    }

    fn array_slot(&self, st: &Value, ty: &Value, parsed: &DynSolType, slot: U256, start: U256, slot_str: &str) -> Option<SlotInfo> {
        let bytes: u64 = s(ty, "numberOfBytes").parse().ok()?;
        let n = bytes.div_ceil(32);
        if slot < start || slot >= start + U256::from(n) {
            return None;
        }
        let index = (slot - start).to::<u64>();
        let label = match parsed {
            DynSolType::FixedArray(inner, _) => match inner.as_ref() {
                DynSolType::FixedArray(_, size) => format!("{}[{}][{}]", s(st, "label"), index / *size as u64, index % *size as u64),
                _ => format!("{}[{index}]", s(st, "label")),
            },
            _ => s(st, "label").to_string(),
        };
        Some(Self::info(label, s(ty, "label"), parsed.clone(), 0, slot_str))
    }

    #[allow(clippy::too_many_arguments)]
    fn struct_slot(&self, base_label: &str, ty: &Value, target: U256, start: U256, offset: i64, slot_str: &str, depth: usize) -> Option<SlotInfo> {
        if depth > 10 {
            return None;
        }
        let members = ty["members"].as_array()?;
        if start == target {
            if let Some(first) = members.iter().find(|m| s(m, "slot") == "0") {
                let first_ty = self.ty(s(first, "type"))?;
                if members.iter().all(|m| s(m, "slot") == "0") {
                    let infos: Vec<SlotInfo> = members
                        .iter()
                        .filter_map(|m| {
                            let mt = self.ty(s(m, "type"))?;
                            Some(Self::info(s(m, "label").into(), s(mt, "label"), parse_type(s(mt, "label"))?, m["offset"].as_i64().unwrap_or(0), slot_str))
                        })
                        .collect();
                    let name = s(ty, "label").strip_prefix("struct ").unwrap_or(s(ty, "label")).to_string();
                    let parsed = DynSolType::CustomStruct {
                        name,
                        prop_names: members.iter().map(|m| s(m, "label").to_string()).collect(),
                        tuple: infos.iter().map(|i| i.ty.clone()).collect(),
                    };
                    let mut info = Self::info(base_label.into(), s(ty, "label"), parsed, offset, slot_str);
                    info.members = (!infos.is_empty()).then_some(infos);
                    return Some(info);
                }
                let label = format!("{base_label}.{}", s(first, "label"));
                if is_struct(s(first_ty, "label")) {
                    return self.struct_slot(&label, first_ty, target, start, first["offset"].as_i64().unwrap_or(0), slot_str, depth + 1);
                }
                return Some(Self::info(label, s(first_ty, "label"), parse_type(s(first_ty, "label"))?, first["offset"].as_i64().unwrap_or(0), slot_str));
            }
        }
        for m in members {
            let m_slot = start + s(m, "slot").parse::<U256>().ok()?;
            let mt = self.ty(s(m, "type"))?;
            let label = format!("{base_label}.{}", s(m, "label"));
            if is_struct(s(mt, "label")) {
                let info = self.struct_slot(&label, mt, target, m_slot, m["offset"].as_i64().unwrap_or(0), slot_str, depth + 1);
                if m_slot == target || info.is_some() {
                    return info;
                }
            }
            if m_slot == target {
                return Some(Self::info(label, s(mt, "label"), parse_type(s(mt, "label"))?, m["offset"].as_i64().unwrap_or(0), slot_str));
            }
        }
        None
    }

    fn mapping_types(&self, id: &str) -> Option<(Vec<String>, String, String)> {
        let ty = self.ty(id)?;
        if s(ty, "encoding") != "mapping" {
            return Some((vec![], s(ty, "label").into(), s(ty, "label").into()));
        }
        let key = self.ty(s(ty, "key"))?;
        let mut keys = vec![s(key, "label").to_string()];
        let value = self.ty(s(ty, "value"))?;
        if s(value, "encoding") == "mapping" {
            let (nested, last, _) = self.mapping_types(s(ty, "value"))?;
            keys.extend(nested);
            return Some((keys, last, s(ty, "label").into()));
        }
        Some((keys, s(value, "label").into(), s(ty, "label").into()))
    }

    fn mapping_slot(&self, st: &Value, ty: &Value, slot: &B256, slot_str: &str, m: &MappingSlots) -> Option<SlotInfo> {
        if s(ty, "encoding") != "mapping" || !m.keys.contains_key(slot) {
            return None;
        }
        let base = B256::from(s(st, "slot").parse::<U256>().ok()?);
        let mut current = *slot;
        let mut keys = vec![];
        let mut found = false;
        while let (Some(k), Some(p)) = (m.keys.get(&current), m.parent_slots.get(&current)) {
            keys.push(*k);
            if *p == base {
                found = true;
                break;
            }
            current = *p;
        }
        if !found {
            return None;
        }
        let (key_types, value_label, full_label) = self.mapping_types(s(st, "type"))?;
        keys.reverse();
        let mut label = s(st, "label").to_string();
        let mut decoded = vec![];
        for (i, k) in keys.iter().enumerate() {
            let shown = key_types.get(i).and_then(|t| parse_type(t)).and_then(|t| t.abi_decode(&k.0).ok()).map(|v| format_raw(&v)).unwrap_or_else(|| hex::encode_prefixed(k.0));
            label = format!("{label}[{shown}]");
            decoded.push(shown);
        }
        let mut info = Self::info(label, &full_label, parse_type(&value_label).unwrap_or(DynSolType::Bytes), st["offset"].as_i64().unwrap_or(0), slot_str);
        info.keys = Some(decoded);
        Some(info)
    }
}
