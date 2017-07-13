extern crate byteorder;
extern crate sgxs as sgxs_crate;
extern crate sgx_isa;

use std::fs::{File, OpenOptions};
use std::io::{Result as IoResult, Error as IoError, Read, Write, Seek, SeekFrom, Cursor, self};
use std::env;
use std::borrow::Cow;
use std::ffi::{OsString, OsStr};
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::cell::RefCell;

use byteorder::{WriteBytesExt, LittleEndian};

use sgx_isa::{PageType, secinfo_flags, SecinfoFlags};
use sgxs_crate::sgxs::{CanonicalSgxsReader, SgxsRead, SgxsWrite, Meas, PageChunk, SecinfoTruncated, Error as SgxsError};
use sgxs_crate::util::size_fit_natural;

enum MainError {
    Usage(Cow<'static, str>),
    Str(Cow<'static, str>),
    Io(IoError, Cow<'static, str>),
    Sgxs(SgxsError, Cow<'static, str>)
}

impl From<&'static str> for MainError {
    fn from(s: &'static str) -> Self {
        MainError::Str(s.into())
    }
}

impl From<(IoError, &'static str)> for MainError {
    fn from((e, s): (IoError, &'static str)) -> Self {
        MainError::Io(e, s.into())
    }
}

impl From<(IoError, String)> for MainError {
    fn from((e, s): (IoError, String)) -> Self {
        MainError::Io(e, s.into())
    }
}

impl From<(SgxsError, &'static str)> for MainError {
    fn from((e, s): (SgxsError, &'static str)) -> Self {
        MainError::Sgxs(e, s.into())
    }
}

impl From<(SgxsError, String)> for MainError {
    fn from((e, s): (SgxsError, String)) -> Self {
        MainError::Sgxs(e, s.into())
    }
}

trait ResultExt<T, E> {
    fn context<C>(self, ctx: C) -> Result<T, (E, C)>;
}

impl<T, E> ResultExt<T, E> for Result<T, E> {
    fn context<C>(self, ctx: C) -> Result<T, (E, C)> {
        self.map_err(|e|(e, ctx))
    }
}

struct NamedFile {
    file: File,
    name: OsString,
}

fn file_error(s: &str, p: &OsStr) -> String {
    format!("Unable to {} `{}'", s, p.to_string_lossy())
}

impl NamedFile {
    fn open_r(p: OsString) -> Result<Self, MainError> {
        let file = File::open(&p).context(file_error("open", &*p))?;
        Ok(NamedFile { file, name: p })
    }

    fn open_rw(p: OsString, w: bool) -> Result<Self, MainError> {
        let file = OpenOptions::new().read(true).write(w).open(&p).context(file_error("open", &*p))?;
        Ok(NamedFile { file, name: p })
    }

    fn error(&self, s: &str) -> String {
        file_error(s, &*self.name)
    }
}

impl Deref for NamedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.file
    }
}

impl DerefMut for NamedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

enum Operation {
    File { perm: SecinfoFlags, measured: bool, file: NamedFile },
    Align(u64),
}

fn parse_op(arg: OsString, next_arg: Option<OsString>) -> Result<Operation, MainError> {
    let arg = arg.to_str().ok_or(MainError::Usage(format!("Unable to parse `{}': expected -<mode> or -align", arg.to_string_lossy()).into()))?;
    let param = next_arg.ok_or(MainError::Usage(format!("After `{}': expected parameter", arg).into()))?;
    if arg == "-align" {
        let align = param.to_str().and_then(|s|s.parse::<u64>().ok())
            .ok_or(MainError::Usage(format!("Unable to parse `{}': expected unsigned integer", param.to_string_lossy()).into()))?;
        Ok(Operation::Align(align))
    } else {
        let mut argchars = arg.chars();
        if argchars.next() != Some('-') {
            return Err(MainError::Usage(format!("Unable to parse `{}': expected -<mode> or -align", arg).into()));
        }
        let mut perm = SecinfoFlags::from(PageType::Reg);
        let mut measured = false;
        for flag in argchars {
            match flag {
                'r' => perm.insert(secinfo_flags::R),
                'w' => perm.insert(secinfo_flags::W),
                'x' => perm.insert(secinfo_flags::X),
                'm' => measured = true,
                c => return Err(MainError::Usage(format!("Unable to parse `{}': got `{}', expected `m', `r', `w', or `x'", arg, c).into()))
            }
        }
        let file = NamedFile::open_r(param)?;
        Ok(Operation::File{perm, measured, file})
    }
}

fn parse_args() -> Result<(NamedFile, bool, Vec<Operation>), MainError> {
    let mut args = env::args_os();
    args.next();
    let (in_place, f0) = match args.next() {
        Some(ref a) if a.to_str() == Some("-i") => (true, args.next()),
        Some(ref a) if a.to_str() == Some("--") => (false, args.next()),
        f0 => (false, f0),
    };
    let f0 = f0.ok_or(MainError::Usage("Must specify file0".into()))?;
    let f0 = NamedFile::open_rw(f0, in_place)?;
    let mut ops = vec![];
    while let Some(arg) = args.next() {
        ops.push(parse_op(arg, args.next())?);
    }
    Ok((f0, in_place, ops))
}

fn result_main() -> Result<(), MainError> {
    trait ReadWriteSeek: Read + Write + Seek {}
    impl<T: Read + Write + Seek + ?Sized> ReadWriteSeek for T {}
    struct SharedRws<T: ReadWriteSeek + ?Sized>(Rc<RefCell<T>>);
    impl<T: ReadWriteSeek + ?Sized> Clone for SharedRws<T> {
        fn clone(&self) -> Self {
            SharedRws(self.0.clone())
        }
    }
    impl<T: ReadWriteSeek + ?Sized> Read for SharedRws<T> {
        fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
            self.0.borrow_mut().read(buf)
        }
    }
    impl<T: ReadWriteSeek + ?Sized> Write for SharedRws<T> {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            self.0.borrow_mut().write(buf)
        }
        fn flush(&mut self) -> IoResult<()> {
            self.0.borrow_mut().flush()
        }
    }
    impl<T: ReadWriteSeek + ?Sized> Seek for SharedRws<T> {
        fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
            self.0.borrow_mut().seek(pos)
        }
    }

    // Parse arguments
    let (mut f0, in_place, ops) = parse_args()?;

    // Buffer input if necessary (non-seeking input)
    let mut f0 = if in_place {
        SharedRws(Rc::new(RefCell::new(f0.file)) as Rc<RefCell<ReadWriteSeek>>)
    } else {
        let mut buf = vec![];
        f0.read_to_end(&mut buf).context(f0.error("read"))?;
        SharedRws(Rc::new(RefCell::new(Cursor::new(buf))) as Rc<RefCell<ReadWriteSeek>>)
    };

    // First read of SGXS, determine where to write enclave size
    let mut f0c = f0.clone();
    let mut cread = CanonicalSgxsReader::new(&mut f0c);
    let offset = match cread.read_meas().context("reading initial SGXS data")? {
        Some(Meas::Unsized(ecr)) =>
            if (ecr.size & 7) == 0 {
                Ok(ecr.size)
            } else {
                Err("Unsized size offset must be naturally aligned")
            },
        Some(Meas::ECreate(_)) => Err("Can only append to unsized SGXS files"),
        None => Err("Empty SGXS file"),
        _ => unreachable!(),
    }.map_err(|s|MainError::Str(s.into()))?;

    let mut enclave_size_foffset = None;
    let mut last_addr = None;
    while let Some(meas) = cread.read_meas().context("reading SGXS data")? {
        match meas {
            Meas::EAdd(eadd) => last_addr = Some(eadd.offset),
            Meas::EExtend{header:eext,..} =>
                if eext.offset <= offset && offset < (eext.offset + 256) {
                    let pos = f0.seek(SeekFrom::Current(0)).context("Determining enclave size position")?;
                    enclave_size_foffset = Some(pos - 256 + (offset & 0xff));
                },
            _ => unreachable!()
        }
    }

    let mut cur_addr = last_addr.ok_or(MainError::Str("No data found in SGXS".into()))? + 0x1000;
    let enclave_size_foffset = enclave_size_foffset.ok_or(MainError::Str("Unable to find enclave size position in SGXS".into()))?;

    // Append new data
    fn align_to(value: &mut u64, align: u64) {
        if (*value & (align - 1)) != 0 {
            *value &= !(align - 1);
            *value += align;
        }
    }

    let mut last_mode = None;
    const EMPTY_PAGE: [u8; 4096] = [0; 4096];
    const EMPTY_CHUNKS: [PageChunk; 16] = [PageChunk::Skipped; 16];
    let mut page = EMPTY_PAGE;
    let mut chunks = EMPTY_CHUNKS;
    let mut page_addr = cur_addr;
    for op in ops {
        match op {
            Operation::Align(n) => align_to(&mut cur_addr, n),
            Operation::File { perm, measured, mut file } => {
                let align = match (last_mode, perm, measured) {
                    (Some((lp, lm)), np, nm) if (lp, lm) == (np, nm) => 1,
                    (Some((lp, _)), np, _) if lp == np => 0x100,
                    _ => 0x1000,
                };
                align_to(&mut cur_addr, align);
                last_mode = Some((perm, measured));

                loop {
                    if cur_addr >= page_addr + 0x1000 {
                        if (cur_addr & 0xfff) != 0 {
                            panic!("Advanced to address {:x} in another page, but it is not at a page boundary. Previos page = {:x}", cur_addr, page_addr);
                        }
                        if chunks != EMPTY_CHUNKS {
                            f0.write_page((&mut&page[..], chunks), page_addr, SecinfoTruncated{flags:perm}).context("writing SGXS data to output")?;
                        }
                        page_addr = cur_addr;
                        page = EMPTY_PAGE;
                        chunks = EMPTY_CHUNKS;
                    }

                    let mut r = (cur_addr as usize & 0xfff)..0x1000;
                    let n = io::copy(&mut (&mut*file).take((r.end-r.start) as _), &mut &mut page[r.clone()]).context(file.error("read"))? as usize;
                    if n == 0 { break }
                    cur_addr += n as _;
                    r.end = r.start + n;
                    for chunk in &mut chunks[(r.start/0x100)..((r.end + 0xff)/0x100)] {
                        *chunk = if measured { PageChunk::IncludedMeasured } else { PageChunk::Included };
                    }
                }
            }
        }
    }

    if chunks != EMPTY_CHUNKS {
        f0.write_page((&mut&page[..], chunks), page_addr, SecinfoTruncated{flags:last_mode.unwrap().0}).context("writing SGXS data to buffer")?;
        page_addr += 0x1000;
    }

    // Determine and write out enclave size
    let enclave_size = size_fit_natural(page_addr);
    f0.seek(SeekFrom::Start(0)).context("seeking in output file")?;
    match f0.read_meas().context("reading SGXS data")? {
        Some(Meas::Unsized(mut ecr)) => {
            ecr.size = enclave_size;
            f0.seek(SeekFrom::Start(0)).context("seeking in output file")?;
            f0.write_meas(&Meas::ECreate(ecr)).context("writing SGXS data to output")?;
        }
        _ => unreachable!()
    }
    f0.seek(SeekFrom::Start(enclave_size_foffset)).context("seeking in output file")?;
    f0.write_u64::<LittleEndian>(enclave_size).context("writing enclave size to output")?;

    if !in_place {
        f0.seek(SeekFrom::Start(0)).context("seeking in output buffer")?;
        let stdout = io::stdout();
        io::copy(&mut f0, &mut stdout.lock()).context("outputting buffer")?;
    }

    Ok(())
}

fn main() {
    match result_main() {
        Err(MainError::Usage(s)) => {
            println!("Usage:
\tsgxs-append [-i|--] <file0> [-<mode> <file1>|-align <num>] ...

\t-i               Modify <file0> in place.
\t--               Ignored (useful if <file0> is named `-i').
\t-<mode> <file>   Append <file> with mode <mode>. <mode> is any
\t                 combination the flags m, r, w, and x (or no flags). m means
\t                 this memory will be measured. r, w, and x indicate the page
\t                 permissions.
\t-align <num>     Align the start of memory for the next file to <num>. The
\t                 default is 1 byte if the page permissions and measurement
\t                 are the same as the last file, 256 bytes if the page
\t                 permissions are the same but the measurement is different,
\t                 or 4096 if the page permissions are different.

ERROR: {}", s)
        },
        Err(MainError::Str(s)) => {
            println!("ERROR: {}", s)
        }
        Err(MainError::Io(e, s)) => {
            println!("ERROR: {}: {}", s, e)
        }
        Err(MainError::Sgxs(e, s)) => {
            println!("ERROR: {}: {:?}", s, e)
        }
        Ok(()) => return,
    }
    std::process::exit(1);
}
