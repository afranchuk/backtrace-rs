//! Support for symbolication using the `gimli` crate on crates.io
//!
//! This implementation is largely a work in progress and is off by default for
//! all platforms, but it's hoped to be developed over time! Long-term this is
//! intended to wholesale replace the `libbacktrace.rs` implementation.

use self::gimli::read::EndianSlice;
use self::gimli::LittleEndian as Endian;
use self::mmap::Mmap;
use crate::symbolize::ResolveWhat;
use crate::types::BytesOrWideString;
use crate::SymbolName;
use addr2line::gimli;
use core::convert::TryInto;
use core::mem;
use core::u32;
use libc::c_void;
use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::prelude::v1::*;

#[cfg(windows)]
#[path = "gimli/mmap_windows.rs"]
mod mmap;
#[cfg(unix)]
#[path = "gimli/mmap_unix.rs"]
mod mmap;

const MAPPINGS_CACHE_SIZE: usize = 4;

struct Context<'a> {
    dwarf: addr2line::Context<EndianSlice<'a, Endian>>,
    object: Object<'a>,
}

struct Mapping {
    // 'static lifetime is a lie to hack around lack of support for self-referential structs.
    cx: Context<'static>,
    _map: Mmap,
}

fn cx<'data>(object: Object<'data>) -> Option<Context<'data>> {
    fn load_section<'data, S>(obj: &Object<'data>) -> S
    where
        S: gimli::Section<gimli::EndianSlice<'data, Endian>>,
    {
        let data = obj.section(S::section_name()).unwrap_or(&[]);
        S::from(EndianSlice::new(data, Endian))
    }

    let dwarf = addr2line::Context::from_sections(
        load_section(&object),
        load_section(&object),
        load_section(&object),
        load_section(&object),
        load_section(&object),
        load_section(&object),
        load_section(&object),
        load_section(&object),
        load_section(&object),
        gimli::EndianSlice::new(&[], Endian),
    )
    .ok()?;
    Some(Context { dwarf, object })
}

macro_rules! mk {
    (Mapping { $map:expr, $inner:expr }) => {{
        use crate::symbolize::gimli::{Context, Mapping, Mmap};

        fn assert_lifetimes<'a>(_: &'a Mmap, _: &Context<'a>) {}
        assert_lifetimes(&$map, &$inner);
        Mapping {
            // Convert to 'static lifetimes since the symbols should
            // only borrow `map` and we're preserving `map` below.
            cx: unsafe { core::mem::transmute::<Context<'_>, Context<'static>>($inner) },
            _map: $map,
        }
    }};
}

fn mmap(path: &Path) -> Option<Mmap> {
    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len().try_into().ok()?;
    unsafe { Mmap::map(&file, len) }
}

cfg_if::cfg_if! {
    if #[cfg(windows)] {
        // Windows uses COFF object files and currently doesn't implement
        // functionality to load a list of native libraries. This seems to work
        // well enough for the main executable but seems pretty likely to not
        // work for loaded DLLs. For now this seems sufficient, but we may have
        // to extend this over time.
        //
        // Note that the native_libraries loading here simply returns one
        // library encompassing the entire address space. This works naively
        // but likely indicates something about ASLR is busted. Let's try to
        // fix this over time if necessary!

        mod coff;
        use self::coff::Object;

        fn native_libraries() -> Vec<Library> {
            let mut ret = Vec::new();
            if let Ok(path) = std::env::current_exe() {
                let mut segments = Vec::new();
                segments.push(LibrarySegment {
                    stated_virtual_memory_address: 0,
                    len: usize::max_value(),
                });
                ret.push(Library {
                    name: path.into(),
                    segments,
                    bias: 0,
                });
            }
            return ret;
        }
    } else if #[cfg(target_os = "macos")] {
        // macOS uses the Mach-O file format and uses DYLD-specific APIs to
        // load a list of native libraries that are part of the appplication.

        use std::os::unix::prelude::*;
        use std::ffi::{OsStr, CStr};

        mod macho;
        use self::macho::Object;

        #[allow(deprecated)]
        fn native_libraries() -> Vec<Library> {
            let mut ret = Vec::new();
            let images = unsafe { libc::_dyld_image_count() };
            for i in 0..images {
                ret.extend(native_library(i));
            }
            return ret;
        }

        #[allow(deprecated)]
        fn native_library(i: u32) -> Option<Library> {
            use object::macho;
            use object::read::macho::{MachHeader, Segment};
            use object::{Bytes, NativeEndian};

            // Fetch the name of this library which corresponds to the path of
            // where to load it as well.
            let name = unsafe {
                let name = libc::_dyld_get_image_name(i);
                if name.is_null() {
                    return None;
                }
                CStr::from_ptr(name)
            };

            // Load the image header of this library and delegate to `object` to
            // parse all the load commands so we can figure out all the segments
            // involved here.
            let (mut load_commands, endian) = unsafe {
                let header = libc::_dyld_get_image_header(i);
                if header.is_null() {
                    return None;
                }
                match (*header).magic {
                    macho::MH_MAGIC => {
                        let endian = NativeEndian;
                        let header = &*(header as *const macho::MachHeader32<NativeEndian>);
                        let data = core::slice::from_raw_parts(
                            header as *const _ as *const u8,
                            mem::size_of_val(header) + header.sizeofcmds.get(endian) as usize
                        );
                        (header.load_commands(endian, Bytes(data)).ok()?, endian)
                    }
                    macho::MH_MAGIC_64 => {
                        let endian = NativeEndian;
                        let header = &*(header as *const macho::MachHeader64<NativeEndian>);
                        let data = core::slice::from_raw_parts(
                            header as *const _ as *const u8,
                            mem::size_of_val(header) + header.sizeofcmds.get(endian) as usize
                        );
                        (header.load_commands(endian, Bytes(data)).ok()?, endian)
                    }
                    _ => return None,
                }
            };

            // Iterate over the segments and register known regions for segments
            // that we find. Additionally record information bout text segments
            // for processing later, see comments below.
            let mut segments = Vec::new();
            let mut first_text = 0;
            let mut text_fileoff_zero = false;
            while let Some(cmd) = load_commands.next().ok()? {
                if let Some((seg, _)) = cmd.segment_32().ok()? {
                    if seg.name() == b"__TEXT" {
                        first_text = segments.len();
                        if seg.fileoff(endian) == 0 && seg.filesize(endian) > 0 {
                            text_fileoff_zero = true;
                        }
                    }
                    segments.push(LibrarySegment {
                        len: seg.vmsize(endian).try_into().ok()?,
                        stated_virtual_memory_address: seg.vmaddr(endian).try_into().ok()?,
                    });
                }
                if let Some((seg, _)) = cmd.segment_64().ok()? {
                    if seg.name() == b"__TEXT" {
                        first_text = segments.len();
                        if seg.fileoff(endian) == 0 && seg.filesize(endian) > 0 {
                            text_fileoff_zero = true;
                        }
                    }
                    segments.push(LibrarySegment {
                        len: seg.vmsize(endian).try_into().ok()?,
                        stated_virtual_memory_address: seg.vmaddr(endian).try_into().ok()?,
                    });
                }
            }

            // Determine the "slide" for this library which ends up being the
            // bias we use to figure out where in memory objects are loaded.
            // This is a bit of a weird computation though and is the result of
            // trying a few things in the wild and seeing what sticks.
            //
            // The general idea is that the `bias` plus a segment's
            // `stated_virtual_memory_address` is going to be where in the
            // actual address space the segment resides. The other thing we rely
            // on though is that a real address minus the `bias` is the index to
            // look up in the symbol table and debuginfo.
            //
            // It turns out, though, that for system loaded libraries these
            // calculations are incorrect. For native executables, however, it
            // appears correct. Lifting some logic from LLDB's source it has
            // some special-casing for the first `__TEXT` section loaded from
            // file offset 0 with a nonzero size. For whatever reason when this
            // is present it appears to mean that the symbol table is relative
            // to just the vmaddr slide for the library. If it's *not* present
            // then the symbol table is relative to the the vmaddr slide plus
            // the segment's stated address.
            //
            // To handle this situation if we *don't* find a text section at
            // file offset zero then we increase the bias by the first text
            // sections's stated address and decrease all stated addresses by
            // that amount as well. That way the symbol table is always appears
            // relative to the library's bias amount. This appears to have the
            // right results for symbolizing via the symbol table.
            //
            // Honestly I'm not entirely sure whether this is right or if
            // there's something else that should indicate how to do this. For
            // now though this seems to work well enough (?) and we should
            // always be able to tweak this over time if necessary.
            //
            // For some more information see #318
            let mut slide = unsafe { libc::_dyld_get_image_vmaddr_slide(i) as usize };
            if !text_fileoff_zero {
                let adjust = segments[first_text].stated_virtual_memory_address;
                for segment in segments.iter_mut() {
                    segment.stated_virtual_memory_address -= adjust;
                }
                slide += adjust;
            }

            Some(Library {
                name: OsStr::from_bytes(name.to_bytes()).to_owned(),
                segments,
                bias: slide,
            })
        }
    } else if #[cfg(any(
        target_os = "linux",
        target_os = "fuchsia",
    ))] {
        // Other Unix (e.g. Linux) platforms use ELF as an object file format
        // and typically implement an API called `dl_iterate_phdr` to load
        // native libraries.

        use std::os::unix::prelude::*;
        use std::ffi::{OsStr, CStr};

        mod elf;
        use self::elf::Object;

        fn native_libraries() -> Vec<Library> {
            let mut ret = Vec::new();
            unsafe {
                libc::dl_iterate_phdr(Some(callback), &mut ret as *mut _ as *mut _);
            }
            return ret;
        }

        unsafe extern "C" fn callback(
            info: *mut libc::dl_phdr_info,
            _size: libc::size_t,
            vec: *mut libc::c_void,
        ) -> libc::c_int {
            let libs = &mut *(vec as *mut Vec<Library>);
            let name = if (*info).dlpi_name.is_null() || *(*info).dlpi_name == 0{
                if libs.is_empty() {
                    std::env::current_exe().map(|e| e.into()).unwrap_or_default()
                } else {
                    OsString::new()
                }
            } else {
                let bytes = CStr::from_ptr((*info).dlpi_name).to_bytes();
                OsStr::from_bytes(bytes).to_owned()
            };
            let headers = core::slice::from_raw_parts((*info).dlpi_phdr, (*info).dlpi_phnum as usize);
            libs.push(Library {
                name,
                segments: headers
                    .iter()
                    .map(|header| LibrarySegment {
                        len: (*header).p_memsz as usize,
                        stated_virtual_memory_address: (*header).p_vaddr as usize,
                    })
                    .collect(),
                bias: (*info).dlpi_addr as usize,
            });
            0
        }
    } else {
        // Everything else should use ELF, but doesn't know how to load native
        // libraries.

        mod elf;
        use self::elf::Object;

        fn native_libraries() -> Vec<Library> {
            Vec::new()
        }
    }
}

#[derive(Default)]
struct Cache {
    /// All known shared libraries that have been loaded.
    libraries: Vec<Library>,

    /// Mappings cache where we retain parsed dwarf information.
    ///
    /// This list has a fixed capacity for its entire liftime which never
    /// increases. The `usize` element of each pair is an index into `libraries`
    /// above where `usize::max_value()` represents the current executable. The
    /// `Mapping` is corresponding parsed dwarf information.
    ///
    /// Note that this is basically an LRU cache and we'll be shifting things
    /// around in here as we symbolize addresses.
    mappings: Vec<(usize, Mapping)>,
}

struct Library {
    name: OsString,
    /// Segments of this library loaded into memory, and where they're loaded.
    segments: Vec<LibrarySegment>,
    /// The "bias" of this library, typically where it's loaded into memory.
    /// This value is added to each segment's stated address to get the actual
    /// virtual memory address that the segment is loaded into. Additionally
    /// this bias is subtracted from real virtual memory addresses to index into
    /// debuginfo and the symbol table.
    bias: usize,
}

struct LibrarySegment {
    /// The stated address of this segment in the object file. This is not
    /// actually where the segment is loaded, but rather this address plus the
    /// containing library's `bias` is where to find it.
    stated_virtual_memory_address: usize,
    /// The size of ths segment in memory.
    len: usize,
}

// unsafe because this is required to be externally synchronized
pub unsafe fn clear_symbol_cache() {
    Cache::with_global(|cache| cache.mappings.clear());
}

impl Cache {
    fn new() -> Cache {
        Cache {
            mappings: Vec::with_capacity(MAPPINGS_CACHE_SIZE),
            libraries: native_libraries(),
        }
    }

    // unsafe because this is required to be externally synchronized
    unsafe fn with_global(f: impl FnOnce(&mut Self)) {
        // A very small, very simple LRU cache for debug info mappings.
        //
        // The hit rate should be very high, since the typical stack doesn't cross
        // between many shared libraries.
        //
        // The `addr2line::Context` structures are pretty expensive to create. Its
        // cost is expected to be amortized by subsequent `locate` queries, which
        // leverage the structures built when constructing `addr2line::Context`s to
        // get nice speedups. If we didn't have this cache, that amortization would
        // never happen, and symbolicating backtraces would be ssssllllooooowwww.
        static mut MAPPINGS_CACHE: Option<Cache> = None;

        f(MAPPINGS_CACHE.get_or_insert_with(|| Cache::new()))
    }

    fn avma_to_svma(&self, addr: *const u8) -> Option<(usize, *const u8)> {
        self.libraries
            .iter()
            .enumerate()
            .filter_map(|(i, lib)| {
                // First up, test if this `lib` has any segment containing the
                // `addr` (handling relocation). If this check passes then we
                // can continue below and actually translate the address.
                if !lib.segments.iter().any(|s| {
                    let svma = s.stated_virtual_memory_address as usize;
                    let start = svma + lib.bias as usize;
                    let end = start + s.len;
                    let address = addr as usize;
                    start <= address && address < end
                }) {
                    return None;
                }

                // Now that we know `lib` contains `addr`, we can offset with
                // the bias to find the stated virutal memory address.
                let svma = addr as usize - lib.bias as usize;
                Some((i, svma as *const u8))
            })
            .next()
    }

    fn mapping_for_lib<'a>(&'a mut self, lib: usize) -> Option<&'a Context<'a>> {
        let idx = self.mappings.iter().position(|(idx, _)| *idx == lib);

        // Invariant: after this conditional completes without early returning
        // from an error, the cache entry for this path is at index 0.

        if let Some(idx) = idx {
            // When the mapping is already in the cache, move it to the front.
            if idx != 0 {
                let entry = self.mappings.remove(idx);
                self.mappings.insert(0, entry);
            }
        } else {
            // When the mapping is not in the cache, create a new mapping,
            // insert it into the front of the cache, and evict the oldest cache
            // entry if necessary.
            let name = &self.libraries[lib].name;
            let mapping = Mapping::new(name.as_ref())?;

            if self.mappings.len() == MAPPINGS_CACHE_SIZE {
                self.mappings.pop();
            }

            self.mappings.insert(0, (lib, mapping));
        }

        let cx: &'a Context<'static> = &self.mappings[0].1.cx;
        // don't leak the `'static` lifetime, make sure it's scoped to just
        // ourselves
        Some(unsafe { mem::transmute::<&'a Context<'static>, &'a Context<'a>>(cx) })
    }
}

pub unsafe fn resolve(what: ResolveWhat, cb: &mut FnMut(&super::Symbol)) {
    let addr = what.address_or_ip();
    let mut call = |sym: Symbol| {
        // Extend the lifetime of `sym` to `'static` since we are unfortunately
        // required to here, but it's ony ever going out as a reference so no
        // reference to it should be persisted beyond this frame anyway.
        let sym = mem::transmute::<Symbol, Symbol<'static>>(sym);
        (cb)(&super::Symbol { inner: sym });
    };

    Cache::with_global(|cache| {
        let (lib, addr) = match cache.avma_to_svma(addr as *const u8) {
            Some(pair) => pair,
            None => return,
        };

        // Finally, get a cached mapping or create a new mapping for this file, and
        // evaluate the DWARF info to find the file/line/name for this address.
        let cx = match cache.mapping_for_lib(lib) {
            Some(cx) => cx,
            None => return,
        };
        let mut any_frames = false;
        if let Ok(mut frames) = cx.dwarf.find_frames(addr as u64) {
            while let Ok(Some(frame)) = frames.next() {
                any_frames = true;
                call(Symbol::Frame {
                    addr: addr as *mut c_void,
                    location: frame.location,
                    name: frame.function.map(|f| f.name.slice()),
                });
            }
        }

        if !any_frames {
            if let Some(name) = cx.object.search_symtab(addr as u64) {
                call(Symbol::Symtab {
                    addr: addr as *mut c_void,
                    name,
                });
            }
        }
    });
}

pub enum Symbol<'a> {
    /// We were able to locate frame information for this symbol, and
    /// `addr2line`'s frame internally has all the nitty gritty details.
    Frame {
        addr: *mut c_void,
        location: Option<addr2line::Location<'a>>,
        name: Option<&'a [u8]>,
    },
    /// Couldn't find debug information, but we found it in the symbol table of
    /// the elf executable.
    Symtab { addr: *mut c_void, name: &'a [u8] },
}

impl Symbol<'_> {
    pub fn name(&self) -> Option<SymbolName> {
        match self {
            Symbol::Frame { name, .. } => {
                let name = name.as_ref()?;
                Some(SymbolName::new(name))
            }
            Symbol::Symtab { name, .. } => Some(SymbolName::new(name)),
        }
    }

    pub fn addr(&self) -> Option<*mut c_void> {
        match self {
            Symbol::Frame { addr, .. } => Some(*addr),
            Symbol::Symtab { .. } => None,
        }
    }

    pub fn filename_raw(&self) -> Option<BytesOrWideString> {
        match self {
            Symbol::Frame { location, .. } => {
                let file = location.as_ref()?.file?;
                Some(BytesOrWideString::Bytes(file.as_bytes()))
            }
            Symbol::Symtab { .. } => None,
        }
    }

    pub fn filename(&self) -> Option<&Path> {
        match self {
            Symbol::Frame { location, .. } => {
                let file = location.as_ref()?.file?;
                Some(Path::new(file))
            }
            Symbol::Symtab { .. } => None,
        }
    }

    pub fn lineno(&self) -> Option<u32> {
        match self {
            Symbol::Frame { location, .. } => location.as_ref()?.line,
            Symbol::Symtab { .. } => None,
        }
    }
}
