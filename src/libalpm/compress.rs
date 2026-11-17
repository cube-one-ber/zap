//! Decoders for the rarer package compressions libarchive reads: Unix
//! `compress` (.Z) natively, as archive_read_support_filter_compress.c, and
//! lrzip and lzop through their programs, as Arch's libarchive does.
use std::io::{BufRead, Read};
use std::process::{Child, ChildStdout, Command, Stdio};

/// LZW-decompress a `compress` stream, whose two magic bytes have been
/// checked but not consumed.
pub(crate) struct Lzw<R> {
    input: R,
    bit_buffer: u32,
    bits_avail: u32,
    bytes_in_section: u32,
    bits: u32,
    maxcode_bits: u32,
    maxcode: u32,
    section_end_code: u32,
    use_reset_code: bool,
    free_ent: u32,
    oldcode: Option<u32>,
    finbyte: u8,
    prefix: Vec<u16>,
    suffix: Vec<u8>,
    /// Decoded bytes not yet returned, in reverse order.
    stack: Vec<u8>,
    done: bool,
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_string())
}

impl<R: Read> Lzw<R> {
    pub(crate) fn new(mut input: R) -> std::io::Result<Lzw<R>> {
        let mut header = [0u8; 3];
        input.read_exact(&mut header)?;
        let maxcode_bits = (header[2] & 0x1f) as u32;
        if !(9..=16).contains(&maxcode_bits) {
            return Err(invalid("Invalid compressed data"));
        }
        let use_reset_code = header[2] & 0x80 != 0;
        Ok(Lzw {
            input,
            bit_buffer: 0,
            bits_avail: 0,
            bytes_in_section: 0,
            bits: 9,
            maxcode_bits,
            maxcode: 1 << maxcode_bits,
            section_end_code: (1 << 9) - 1,
            use_reset_code,
            free_ent: if use_reset_code { 257 } else { 256 },
            oldcode: None,
            finbyte: 0,
            prefix: vec![0; 1 << 16],
            suffix: (0..=255u8).chain(std::iter::repeat_n(0, (1 << 16) - 256)).collect(),
            stack: Vec::new(),
            done: false,
        })
    }

    /// The next `n` bits, low bits first; None at the end of the input.
    fn getbits(&mut self, n: u32) -> std::io::Result<Option<u32>> {
        while self.bits_avail < n {
            let mut byte = [0u8; 1];
            if self.input.read(&mut byte)? == 0 {
                return Ok(None);
            }
            self.bit_buffer |= (byte[0] as u32) << self.bits_avail;
            self.bits_avail += 8;
            self.bytes_in_section += 1;
        }
        let code = self.bit_buffer & ((1 << n) - 1);
        self.bit_buffer >>= n;
        self.bits_avail -= n;
        Ok(Some(code))
    }

    /// Decode one code onto the stack; false at the end of the input.
    fn next_code(&mut self) -> std::io::Result<bool> {
        let Some(mut code) = self.getbits(self.bits)? else { return Ok(false) };
        let newcode = code;
        if code == 256 && self.use_reset_code {
            // A reset skips to the end of the current block of codes.
            let skip = (self.bits - self.bytes_in_section % self.bits) % self.bits;
            self.bits_avail = 0;
            self.bit_buffer = 0;
            for _ in 0..skip {
                if self.getbits(8)?.is_none() {
                    return Ok(false);
                }
            }
            self.bytes_in_section = 0;
            self.bits = 9;
            self.section_end_code = (1 << 9) - 1;
            self.free_ent = 257;
            self.oldcode = None;
            return self.next_code();
        }
        if code > self.free_ent || (code == self.free_ent && self.oldcode.is_none()) {
            return Err(invalid("Invalid compressed data"));
        }
        let mut out = Vec::new();
        // The KwKwK case: a code defined by this very step.
        if code >= self.free_ent {
            out.push(self.finbyte);
            code = self.oldcode.unwrap_or(0);
        }
        while code >= 256 {
            out.push(self.suffix[code as usize]);
            code = self.prefix[code as usize] as u32;
        }
        self.finbyte = code as u8;
        out.push(self.finbyte);
        // `out` holds the string backwards, so popping yields it in order.
        // The stack is empty here: `read` decodes only once it has drained.
        self.stack.extend(out);
        let entry = self.free_ent;
        if let Some(old) = self.oldcode.filter(|_| entry < self.maxcode) {
            self.prefix[entry as usize] = old as u16;
            self.suffix[entry as usize] = self.finbyte;
            self.free_ent += 1;
        }
        if self.free_ent > self.section_end_code {
            self.bits += 1;
            self.bytes_in_section = 0;
            self.section_end_code = if self.bits == self.maxcode_bits { self.maxcode } else { (1 << self.bits) - 1 };
        }
        self.oldcode = Some(newcode);
        Ok(true)
    }
}

impl<R: Read> Read for Lzw<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.stack.is_empty() && !self.done {
            if !self.next_code()? {
                self.done = true;
            }
        }
        let n = buf.len().min(self.stack.len());
        for slot in buf.iter_mut().take(n) {
            *slot = self.stack.pop().unwrap_or(0);
        }
        Ok(n)
    }
}

/// The output of a decompressing program fed `input` from a thread, as
/// libarchive's program filters. A failing program is a read error.
pub(crate) struct Program {
    name: &'static str,
    child: Child,
    output: ChildStdout,
}

impl Program {
    pub(crate) fn spawn(name: &'static str, args: &[&str], mut input: impl BufRead + Send + 'static) -> std::io::Result<Program> {
        let mut child = Command::new(name)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| std::io::Error::other(format!("Can't launch external program: {name}")))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut input, &mut stdin);
        });
        let output = child.stdout.take().expect("piped stdout");
        Ok(Program { name, child, output })
    }
}

impl Read for Program {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.output.read(buf)?;
        if n == 0 && !buf.is_empty() && !self.child.wait()?.success() {
            return Err(std::io::Error::other(format!("{} failed", self.name)));
        }
        Ok(n)
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn compress_streams_decode_through_code_growth_and_dictionary_resets() {
        // Enough varied data for 16-bit codes and several resets.
        let tmp = TempDir::new();
        let mut data = Vec::new();
        let mut x: u32 = 1;
        for i in 0..400_000u32 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            data.push(if i % 3 == 0 { b'a' + (x >> 16) as u8 % 26 } else { (x >> 24) as u8 });
        }
        std::fs::write(tmp.path().join("data"), &data).unwrap();
        let dir = tmp.path().to_str().unwrap();
        for (out, flag) in [("plain.tar", None), ("packed.tar.Z", Some("-Z"))] {
            let out = format!("{dir}/{out}");
            let mut args = vec!["/usr/bin/bsdtar", "-cf", &out];
            args.extend(flag);
            args.extend(["-C", dir, "data"]);
            crate::util::run(&args, None).unwrap();
        }
        let packed = std::fs::File::open(tmp.path().join("packed.tar.Z")).unwrap();
        let mut decoded = Vec::new();
        Lzw::new(std::io::BufReader::new(packed)).unwrap().read_to_end(&mut decoded).unwrap();
        assert!(decoded == std::fs::read(tmp.path().join("plain.tar")).unwrap());
    }
}
