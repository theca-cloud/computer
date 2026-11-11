//! A download handed on in order, in runs: chunks arrive out of order, at
//! any offset; what is contiguous from the start goes out a run at a time
//! (Windows' transfers take 4 KiB-aligned runs, the file's end excepted).

use std::collections::BTreeMap;
use std::io;

#[derive(Debug, Default)]
pub struct Runs {
    /// Handed on up to here.
    sent: u64,
    /// Contiguous from `sent`, not handed on yet.
    ready: Vec<u8>,
    /// Arrived past what is contiguous, by offset.
    later: BTreeMap<u64, Vec<u8>>,
}

impl Runs {
    /// A chunk arrived: whole runs of `run` bytes (a multiple of 4 KiB) go
    /// to `send` (data, offset), in order.
    pub fn arrived(
        &mut self,
        offset: u64,
        data: &[u8],
        run: usize,
        send: impl FnMut(&[u8], u64) -> io::Result<()>,
    ) -> io::Result<()> {
        self.later.insert(offset, data.to_vec());
        loop {
            let next = self.sent + self.ready.len() as u64;
            let Some(chunk) = self.later.remove(&next) else {
                break;
            };
            self.ready.extend_from_slice(&chunk);
        }
        self.send(run, false, send)
    }

    /// Every chunk arrived: the rest goes (the file's end, any length). A
    /// gap is an error.
    pub fn finished(
        &mut self,
        run: usize,
        send: impl FnMut(&[u8], u64) -> io::Result<()>,
    ) -> io::Result<()> {
        if !self.later.is_empty() {
            return Err(io::Error::other("a gap in the download"));
        }
        self.send(run, true, send)
    }

    fn send(
        &mut self,
        run: usize,
        all: bool,
        mut send: impl FnMut(&[u8], u64) -> io::Result<()>,
    ) -> io::Result<()> {
        while self.ready.len() >= run || (all && !self.ready.is_empty()) {
            let n = self.ready.len().min(run);
            send(&self.ready[..n], self.sent)?;
            self.sent += n as u64;
            self.ready.drain(..n);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chunks out of order, of odd sizes: what goes out is the file, in
    /// order, in whole runs but the last.
    #[test]
    fn runs_go_in_order_and_aligned() {
        let file: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let cuts = [0, 3_000, 9_999, 10_000, 17_123, 20_000];
        let chunks: Vec<(u64, &[u8])> = cuts
            .windows(2)
            .map(|w| (w[0] as u64, &file[w[0]..w[1]]))
            .collect();
        let mut out: Vec<(u64, usize)> = Vec::new();
        let mut got = Vec::new();
        let mut runs = Runs::default();
        let mut record = |data: &[u8], offset: u64| {
            assert_eq!(offset as usize, got.len(), "in order");
            got.extend_from_slice(data);
            out.push((offset, data.len()));
            Ok(())
        };
        for i in [3, 1, 4, 0, 2] {
            runs.arrived(chunks[i].0, chunks[i].1, 4096, &mut record)
                .unwrap();
        }
        runs.finished(4096, &mut record).unwrap();
        assert_eq!(got, file);
        let (last, whole) = out.split_last().unwrap();
        assert!(
            whole.iter().all(|(o, n)| o % 4096 == 0 && *n == 4096),
            "{out:?}"
        );
        assert_eq!(last.0 + last.1 as u64, 20_000);
        let mut gap = Runs::default();
        gap.arrived(4096, &[1; 10], 4096, |_, _| Ok(())).unwrap();
        assert!(gap.finished(4096, |_, _| Ok(())).is_err());
    }
}
