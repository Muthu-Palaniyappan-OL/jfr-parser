//! Low level decoding of the JFR wire primitives.
//!
//! JFR "compressed integers" are LEB128-style: 7 payload bits per byte, the
//! high bit means "more follows", and a 64-bit value takes at most 9 bytes
//! where the 9th byte carries a full 8 bits. This matches `RecordingInput` in
//! the JDK and `JfrReader` in async-profiler. Padded encodings (extra
//! continuation bytes with zero payload, used for size fields patched after
//! the fact) decode correctly.
//!
//! Floats and doubles are raw big-endian. Booleans and bytes are one byte.
//! Shorts, chars, ints and longs are all varints.

use crate::error::{Result, corrupt, truncated};
use std::borrow::Cow;

/// Decodes one varint at `pos`. Returns the value and the position after it.
#[inline(always)]
pub fn varlong_at(buf: &[u8], pos: usize) -> Result<(u64, usize)> {
    if let Some(w) = pos.checked_add(9).and_then(|end| buf.get(pos..end)) {
        let b0 = w[0];
        if b0 < 0x80 {
            return Ok((b0 as u64, pos + 1));
        }
        let mut v = (b0 & 0x7f) as u64;
        for (i, &b) in w[1..8].iter().enumerate() {
            v |= ((b & 0x7f) as u64) << (7 * (i + 1));
            if b < 0x80 {
                return Ok((v, pos + i + 2));
            }
        }
        v |= (w[8] as u64) << 56;
        return Ok((v, pos + 9));
    }
    varlong_slow(buf, pos)
}

#[cold]
#[inline(never)]
fn varlong_slow(buf: &[u8], mut pos: usize) -> Result<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *buf.get(pos).ok_or_else(|| truncated("varint", pos))?;
        pos += 1;
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b < 0x80 {
            return Ok((v, pos));
        }
    }
    let b = *buf.get(pos).ok_or_else(|| truncated("varint", pos))?;
    Ok((v | (b as u64) << 56, pos + 1))
}

/// Skips `n` consecutive varints starting at `pos` and returns the new position.
///
/// Instead of walking byte by byte, eight bytes are loaded at once and the
/// bytes whose high bit is clear (the varint terminators) are located with a
/// bit mask, the same idea simdjson uses to find structural characters. The
/// n-th terminator gives the end position directly.
#[inline]
pub fn skip_varints(buf: &[u8], mut pos: usize, mut n: usize) -> Result<usize> {
    while n > 0 {
        let Some(chunk) = buf.get(pos..).and_then(|s| s.first_chunk::<8>()) else {
            let (_, p) = varlong_at(buf, pos)?;
            pos = p;
            n -= 1;
            continue;
        };
        let w = u64::from_le_bytes(*chunk);
        let mut term = !w & 0x8080_8080_8080_8080;
        let cnt = term.count_ones() as usize;
        if cnt >= n {
            for _ in 1..n {
                term &= term - 1;
            }
            return Ok(pos + (term.trailing_zeros() >> 3) as usize + 1);
        }
        if cnt == 0 {
            // Eight continuation bytes: a 9-byte varint whose last byte is raw.
            if pos + 9 > buf.len() {
                return Err(truncated("varint", pos));
            }
            pos += 9;
            n -= 1;
        } else {
            // Consume through the last terminator in the window; the partial
            // varint after it is re-examined from its own start.
            pos += (63 - term.leading_zeros() as usize) / 8 + 1;
            n -= cnt;
        }
    }
    if pos > buf.len() {
        return Err(truncated("varint", pos));
    }
    Ok(pos)
}

/// A string as stored in the recording, not yet decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawStr<'a> {
    Null,
    Empty,
    /// Reference into the `java.lang.String` constant pool.
    Pool(u64),
    Utf8(&'a [u8]),
    Latin1(&'a [u8]),
    /// `count` varint-encoded UTF-16 code units in `data`.
    Chars { count: usize, data: &'a [u8] },
}

impl<'a> RawStr<'a> {
    /// Decodes into a `str`, borrowing whenever the bytes are already valid.
    /// Null and pool references yield `None`; resolve pool references through the chunk.
    pub fn to_str(self) -> Option<Cow<'a, str>> {
        Some(match self {
            RawStr::Null | RawStr::Pool(_) => return None,
            RawStr::Empty => Cow::Borrowed(""),
            RawStr::Utf8(b) => String::from_utf8_lossy(b),
            RawStr::Latin1(b) => {
                if b.is_ascii() {
                    match std::str::from_utf8(b) {
                        Ok(s) => Cow::Borrowed(s),
                        Err(_) => String::from_utf8_lossy(b),
                    }
                } else {
                    Cow::Owned(b.iter().map(|&c| c as char).collect())
                }
            }
            RawStr::Chars { count, data } => {
                let mut units = Vec::with_capacity(count);
                let mut p = 0;
                for _ in 0..count {
                    match varlong_at(data, p) {
                        Ok((v, np)) => {
                            units.push(v as u16);
                            p = np;
                        }
                        Err(_) => break,
                    }
                }
                Cow::Owned(String::from_utf16_lossy(&units))
            }
        })
    }
}

/// Parses a string value at `pos`. Returns it with the position after it.
pub fn string_at(buf: &[u8], pos: usize) -> Result<(RawStr<'_>, usize)> {
    let enc = *buf.get(pos).ok_or_else(|| truncated("string", pos))?;
    let pos = pos + 1;
    match enc {
        0 => Ok((RawStr::Null, pos)),
        1 => Ok((RawStr::Empty, pos)),
        2 => {
            let (id, p) = varlong_at(buf, pos)?;
            Ok((RawStr::Pool(id), p))
        }
        3 | 5 => {
            let (len, p) = varlong_at(buf, pos)?;
            let len = usize::try_from(len).map_err(|_| corrupt("string length", pos))?;
            let end = p
                .checked_add(len)
                .filter(|&e| e <= buf.len())
                .ok_or_else(|| truncated("string bytes", p))?;
            let b = &buf[p..end];
            Ok((if enc == 3 { RawStr::Utf8(b) } else { RawStr::Latin1(b) }, end))
        }
        4 => {
            let (count, p) = varlong_at(buf, pos)?;
            let count = usize::try_from(count).map_err(|_| corrupt("string length", pos))?;
            if count > buf.len().saturating_sub(p) {
                return Err(truncated("string chars", p));
            }
            let end = skip_varints(buf, p, count)?;
            Ok((RawStr::Chars { count, data: &buf[p..end] }, end))
        }
        _ => Err(corrupt("unknown string encoding", pos - 1)),
    }
}

/// Skips a string value without decoding it.
#[inline]
pub fn skip_string(buf: &[u8], pos: usize) -> Result<usize> {
    let enc = *buf.get(pos).ok_or_else(|| truncated("string", pos))?;
    let pos = pos + 1;
    match enc {
        0 | 1 => Ok(pos),
        2 => skip_varints(buf, pos, 1),
        3 | 5 => {
            let (len, p) = varlong_at(buf, pos)?;
            usize::try_from(len)
                .ok()
                .and_then(|l| p.checked_add(l))
                .filter(|&e| e <= buf.len())
                .ok_or_else(|| truncated("string bytes", p))
        }
        4 => {
            let (count, p) = varlong_at(buf, pos)?;
            let count = usize::try_from(count).map_err(|_| corrupt("string length", pos))?;
            if count > buf.len().saturating_sub(p) {
                return Err(truncated("string chars", p));
            }
            skip_varints(buf, p, count)
        }
        _ => Err(corrupt("unknown string encoding", pos - 1)),
    }
}

/// A forward-only cursor over a byte slice. Used for the non-hot paths
/// (metadata, headers); hot paths use the free functions with explicit positions.
pub struct Cursor<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8], pos: usize) -> Self {
        Cursor { buf, pos }
    }
    #[inline]
    pub fn varlong(&mut self) -> Result<u64> {
        let (v, p) = varlong_at(self.buf, self.pos)?;
        self.pos = p;
        Ok(v)
    }
    #[inline]
    pub fn long(&mut self) -> Result<i64> {
        Ok(self.varlong()? as i64)
    }
    /// A non-negative count that must fit in the remaining bytes (each
    /// element takes at least one byte), which bounds allocations on bad input.
    pub fn count(&mut self, what: &'static str) -> Result<usize> {
        let at = self.pos;
        let v = self.varlong()?;
        let v = usize::try_from(v).map_err(|_| corrupt(what, at))?;
        if v > self.buf.len().saturating_sub(self.pos) {
            return Err(corrupt(what, at));
        }
        Ok(v)
    }
    pub fn string(&mut self) -> Result<RawStr<'a>> {
        let (s, p) = string_at(self.buf, self.pos)?;
        self.pos = p;
        Ok(s)
    }
}

#[inline(always)]
pub fn f32_at(buf: &[u8], pos: usize) -> Result<f32> {
    pos.checked_add(4)
        .and_then(|e| buf.get(pos..e))
        .map(|b| f32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| truncated("float", pos))
}

#[inline(always)]
pub fn f64_at(buf: &[u8], pos: usize) -> Result<f64> {
    pos.checked_add(8)
        .and_then(|e| buf.get(pos..e))
        .map(|b| f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        .ok_or_else(|| truncated("double", pos))
}

#[inline(always)]
pub fn u8_at(buf: &[u8], pos: usize) -> Result<u8> {
    buf.get(pos).copied().ok_or_else(|| truncated("byte", pos))
}
