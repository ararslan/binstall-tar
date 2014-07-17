//! A library for reading an writing TAR archives
//!
//! This library provides utilities necessary to manage TAR archives [1]
//! abstracted over a reader or writer. Great strides are taken to ensure that
//! an archive is never required to be fully resident in memory, all objects
//! provide largely a streaming interface to read bytes from.
//!
//! [1]: http://en.wikipedia.org/wiki/Tar_%28computing%29

#![feature(macro_rules)]
#![deny(missing_doc)]

use std::cell::{RefCell, Cell};
use std::cmp;
use std::io::{IoResult, IoError};
use std::io;
use std::iter::AdditiveIterator;
use std::mem;
use std::num;
use std::str;

/// A top-level representation of an archive file.
///
/// This archive can have a file added to it and it can be iterated over.
pub struct Archive<R> {
    obj: RefCell<R>,
    pos: Cell<u64>,
}

/// An iterator over the files of an archive.
pub struct Files<'a, R> {
    archive: &'a Archive<R>,
    done: bool,
    offset: u64,
}

/// A read-only view into a file of an archive.
///
/// This structure is a windows into a portion of a borrowed archive which can
/// be inspected. It acts as a file handle by implementing the Reader and Seek
/// traits. A file cannot be rewritten once inserted into an archive.
pub struct File<'a, R> {
    header: Header,
    archive: &'a Archive<R>,
    tar_offset: u64,
    pos: u64,
    size: u64,
}

#[repr(C)]
struct Header {
    name: [u8, ..100],
    mode: [u8, ..8],
    owner: [u8, ..8],
    group: [u8, ..8],
    size: [u8, ..12],
    mtime: [u8, ..12],
    cksum: [u8, ..8],
    link: [u8, ..1],
    linkname: [u8, ..100],
    _rest: [u8, ..255],
}

impl<O> Archive<O> {
    /// Create a new archive with the underlying object as the reader/writer.
    ///
    /// Different methods are available on an archive depending on the traits
    /// that the underlying object implements.
    pub fn new(obj: O) -> Archive<O> {
        Archive { obj: RefCell::new(obj), pos: Cell::new(0) }
    }
}

impl<R: Seek + Reader> Archive<R> {
    /// Construct an iterator over the files of this archive.
    ///
    /// This function can return an error if any underlying I/O operation files
    /// while attempting to construct the iterator.
    ///
    /// Additionally, the iterator yields `IoResult<File>` instead of `File` to
    /// handle invalid tar archives as well as any intermittent I/O error that
    /// occurs.
    pub fn files<'a>(&'a self) -> IoResult<Files<'a, R>> {
        try!(self.seek(0));
        Ok(Files { archive: self, done: false, offset: 0 })
    }

    fn seek(&self, pos: u64) -> IoResult<()> {
        if self.pos.get() == pos { return Ok(()) }
        try!(self.obj.borrow_mut().seek(pos as i64, io::SeekSet));
        self.pos.set(pos);
        Ok(())
    }
}

impl<'a, R: Seek + Reader> Iterator<IoResult<File<'a, R>>> for Files<'a, R> {
    fn next(&mut self) -> Option<IoResult<File<'a, R>>> {
        macro_rules! try( ($e:expr) => (
            match $e {
                Ok(e) => e,
                Err(e) => { self.done = true; return Some(Err(e)) }
            }
        ) )
        macro_rules! bail( () => ({
            self.done = true;
            return Some(Err(bad_archive()))
        }) )

        // If we hit a previous error, or we reached the end, we're done here
        if self.done { return None }

        // Make sure that we've seeked to the start of the next file in this
        // iterator, and then parse the chunk. If we have 2 or more sections of
        // all 0s, then the archive is done.
        try!(self.archive.seek(self.offset));
        let mut chunk = [0, ..512];
        let mut cnt = 0i;
        loop {
            if try!(self.archive.read(chunk)) != 512 {
                bail!()
            }
            self.offset += 512;
            if chunk.iter().any(|i| *i != 0) { break }
            cnt += 1;
            if cnt > 1 {
                self.done = true;
                return None
            }
        }

        let sum = chunk.slice_to(148).iter().map(|i| *i as uint).sum() +
                  chunk.slice_from(156).iter().map(|i| *i as uint).sum() +
                  32 * 8;

        let hd: Header = unsafe { mem::transmute(chunk) };
        let mut ret = File {
            archive: self.archive,
            header: hd,
            pos: 0,
            size: 0,
            tar_offset: self.offset,
        };

        // Make sure the checksum is ok
        let cksum = try!(ret.cksum());
        if sum != cksum { bail!() }

        // Figure out where the next file is
        let size = try!(ret.calc_size());
        ret.size = size;
        let size = (size + 511) & !(512 - 1);
        self.offset += size;

        Some(Ok(ret))
    }
}

impl<'a, R: Seek + Reader> File<'a, R> {
    /// Returns the filename of this archive as a byte array
    pub fn filename_bytes<'a>(&'a self) -> &'a [u8] { truncate(self.header.name) }

    /// Returns the filename of this archive as a utf8 string.
    ///
    /// If `None` is returned, then the filename is not valid utf8
    pub fn filename<'a>(&'a self) -> Option<&'a str> {
        str::from_utf8(self.filename_bytes())
    }

    /// Returns the size of the file in the archive.
    pub fn size(&self) -> u64 { self.size }

    fn calc_size(&self) -> IoResult<u64> {
        let num = match str::from_utf8(truncate(self.header.size)) {
            Some(n) => n,
            None => return Err(bad_archive()),
        };
        match num::from_str_radix(num, 8) {
            Some(n) => Ok(n),
            None => Err(bad_archive())
        }
    }

    fn cksum(&self) -> IoResult<uint> {
        let num = match str::from_utf8(truncate(self.header.cksum)) {
            Some(n) => n,
            None => return Err(bad_archive())
        };
        match num::from_str_radix(num.trim(), 8) {
            Some(n) => Ok(n),
            None => Err(bad_archive())
        }
    }
}

impl<'a, R: Reader> Reader for &'a Archive<R> {
    fn read(&mut self, into: &mut [u8]) -> IoResult<uint> {
        self.obj.borrow_mut().read(into).map(|i| {
            self.pos.set(self.pos.get() + i as u64);
            i
        })
    }
}

impl<'a, R: Reader + Seek> Reader for File<'a, R> {
    fn read(&mut self, into: &mut [u8]) -> IoResult<uint> {
        if self.size == self.pos {
            return Err(io::standard_error(io::EndOfFile))
        }

        try!(self.archive.seek(self.tar_offset + self.pos));

        let amt = cmp::min((self.size - self.pos) as uint, into.len());
        let amt = try!(self.archive.read(into.mut_slice_to(amt)));
        self.pos += amt as u64;
        Ok(amt)
    }
}

impl<'a, R> Seek for File<'a, R> {
    fn tell(&self) -> IoResult<u64> { Ok(self.pos) }
    fn seek(&mut self, pos: i64, style: io::SeekStyle) -> IoResult<()> {
        let next = match style {
            io::SeekSet => pos as i64,
            io::SeekCur => self.pos as i64 + pos,
            io::SeekEnd => self.size as i64 + pos,
        };
        if next < 0 {
            Err(io::standard_error(io::OtherIoError))
        } else if next as u64 > self.size {
            Err(io::standard_error(io::OtherIoError))
        } else {
            self.pos = next as u64;
            Ok(())
        }
    }
}

fn bad_archive() -> IoError {
    IoError {
        kind: io::OtherIoError,
        desc: "invalid tar archive",
        detail: None,
    }
}

fn truncate<'a>(slice: &'a [u8]) -> &'a [u8] {
    match slice.iter().position(|i| *i == 0) {
        Some(i) => slice.slice_to(i),
        None => slice,
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::io::BufReader;
    use super::Archive;

    #[test]
    fn simple() {
        let rdr = BufReader::new(include_bin!("tests/simple.tar"));
        let ar = Archive::new(rdr);
        for file in ar.files().unwrap() {
            file.unwrap();
        }
    }

    #[test]
    fn reading_files() {
        let rdr = BufReader::new(include_bin!("tests/reading_files.tar"));
        let ar = Archive::new(rdr);
        let mut files = ar.files().unwrap();
        let mut a = files.next().unwrap().unwrap();
        let mut b = files.next().unwrap().unwrap();
        assert!(files.next().is_none());

        assert_eq!(a.filename(), Some("a"));
        assert_eq!(b.filename(), Some("b"));
        assert_eq!(a.read_to_string().unwrap().as_slice(),
                   "a\na\na\na\na\na\na\na\na\na\na\n");
        assert_eq!(b.read_to_string().unwrap().as_slice(),
                   "b\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\n");
        a.seek(0, io::SeekSet).unwrap();
        assert_eq!(a.read_to_string().unwrap().as_slice(),
                   "a\na\na\na\na\na\na\na\na\na\na\n");
    }
}