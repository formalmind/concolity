//! solc's compressed source maps: `s:l:f:j:m` per instruction, separated by `;`, where an empty field repeats the
//! previous entry's value. Instruction index, not pc: a `PUSHn` takes 1 + n bytes but one entry.

/// One instruction's place in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrcEntry {
    /// Byte offset into the source file.
    pub start: i64,
    /// Byte length.
    pub length: i64,
    /// Source id (`sources.<path>.id`); -1 when the compiler generated the instruction.
    pub file: i64,
    /// `i` into a function, `o` out of one, `-` a plain jump (or not a jump).
    pub jump: Jump,
    /// Modifier depth (solc ≥ 0.6): which `_` of a modifier the instruction runs under.
    pub modifier_depth: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jump {
    In,
    Out,
    Regular,
}

impl SrcEntry {
    pub fn has_source(&self) -> bool {
        self.file >= 0 && self.start >= 0
    }
}

/// Decodes a compressed source map into one entry per instruction.
pub fn decode(map: &str) -> Vec<SrcEntry> {
    let mut out = Vec::new();
    let mut prev = SrcEntry { start: -1, length: -1, file: -1, jump: Jump::Regular, modifier_depth: 0 };
    if map.is_empty() {
        return out;
    }
    for item in map.split(';') {
        let mut e = prev;
        for (i, field) in item.split(':').enumerate() {
            if field.is_empty() {
                continue;
            }
            match i {
                0 => e.start = field.parse().unwrap_or(-1),
                1 => e.length = field.parse().unwrap_or(-1),
                2 => e.file = field.parse().unwrap_or(-1),
                3 => {
                    e.jump = match field {
                        "i" => Jump::In,
                        "o" => Jump::Out,
                        _ => Jump::Regular,
                    }
                }
                4 => e.modifier_depth = field.parse().unwrap_or(0),
                _ => {}
            }
        }
        out.push(e);
        prev = e;
    }
    out
}

/// pc → instruction index for a piece of bytecode. Bytes inside a PUSH's immediate (and past the end of the code)
/// map to `usize::MAX`.
pub fn pc_to_index(code: &[u8]) -> Vec<usize> {
    let mut map = vec![usize::MAX; code.len() + 1];
    let (mut pc, mut ix) = (0usize, 0usize);
    while pc < code.len() {
        map[pc] = ix;
        let op = code[pc];
        // PUSH1..PUSH32 are 0x60..0x7f.
        let imm = if (0x60..=0x7f).contains(&op) { (op - 0x5f) as usize } else { 0 };
        pc += 1 + imm;
        ix += 1;
    }
    // A STOP that runs off the end of the code (pc == len) is the instruction after the last one.
    map[code.len()] = ix;
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_compressed_entries() {
        let m = decode("10:5:0:-:0;;12;:3:1:i;-1:-1:-1:o");
        assert_eq!(m.len(), 5);
        assert_eq!((m[0].start, m[0].length, m[0].file), (10, 5, 0));
        assert_eq!(m[1], m[0]);
        assert_eq!((m[2].start, m[2].length, m[2].file), (12, 5, 0));
        assert_eq!((m[3].start, m[3].length, m[3].file, m[3].jump), (12, 3, 1, Jump::In));
        assert!(!m[4].has_source());
        assert_eq!(m[4].jump, Jump::Out);
    }

    #[test]
    fn push_immediates_share_an_index() {
        // PUSH1 0x80 PUSH1 0x40 MSTORE PUSH2 0x0102 STOP
        let code = [0x60, 0x80, 0x60, 0x40, 0x52, 0x61, 0x01, 0x02, 0x00];
        let m = pc_to_index(&code);
        assert_eq!((m[0], m[2], m[4], m[5], m[8]), (0, 1, 2, 3, 4));
        assert_eq!(m[1], usize::MAX);
    }
}
