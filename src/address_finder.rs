pub use self::os_impl::*;
use libc::pid_t;

/*
 * Operating-system specific code for getting
 * a) the address of the current thread, and
 * b) the address of the Ruby version of a PID
 *
 * from a running Ruby process. Involves a lot of reading memory maps and symbols.
 */

#[derive(Fail, Debug)]
pub enum AddressFinderError {
    #[fail(display = "No process with PID: {}", _0)] NoSuchProcess(pid_t),
    #[fail(display = "Permission denied when reading from process {}. Try again with sudo?", _0)]
    PermissionDenied(pid_t),
    #[fail(display = "Couldn't get port for PID {}. Possibilities: that process doesn't exist, you need to be root, or you have SIP enabled and you're trying to profile system Ruby (try rbenv instead).", _0)]
    MacPermissionDenied(pid_t),
    #[fail(display = "Error reading /proc/{}/maps", _0)] ProcMapsError(pid_t),
}

#[cfg(target_os = "macos")]
mod os_impl {
    use address_finder::AddressFinderError;
    use goblin::mach;
    use failure::Error;
    use std;
    use std::io::Read;
    use libc::pid_t;
    use proc_maps::MapRange;
    use read_process_memory::*;
    use mac_maps::{get_process_maps, task_for_pid, MacMapRange};

    pub fn get_ruby_version_address(pid: pid_t) -> Result<usize, Error> {
        let binary = &get_ruby_binary(pid)?;
        binary.symbol_addr("_ruby_version")
    }

    pub fn current_thread_address(
        pid: pid_t,
        version: &str,
        _is_maybe_thread: Box<Fn(usize, &ProcessHandle, &Vec<MapRange>) -> bool>,
    ) -> Result<usize, Error> {
        let binary = &get_ruby_binary(pid)?;
        if version >= "2.5.0" {
            binary.symbol_addr("_ruby_current_execution_context_ptr")
        } else {
            binary.symbol_addr("_ruby_current_thread")
        }
    }

    struct Binary {
        pub start_addr: usize,
        pub mach: Vec<u8>,
    }

    impl Binary {
        pub fn from(start_addr: usize, filename: &str) -> Result<Binary, Error> {
            let mut file = std::fs::File::open(&filename)?;
            let mut contents: Vec<u8> = Vec::new();
            file.read_to_end(&mut contents)?;
            Ok(Binary {
                start_addr: start_addr,
                mach: contents,
            })
        }

        pub fn symbol_addr(&self, symbol_name: &str) -> Result<usize, Error> {
            let base_address = self.symbol_value_mach("__mh_execute_header")?;
            let addr = self.symbol_value_mach(symbol_name)?;
            Ok(addr + self.start_addr - base_address)
        }

        // Gets the value of a symbol from the Mach-O binary
        // TODO: don't repeatedly parse the Mach-O binary every time
        // haven't done this yet because of lifetime annoyances
        pub fn symbol_value_mach(&self, symbol_name: &str) -> Result<usize, Error> {
            let mach = match mach::Mach::parse(&self.mach) {
                Ok(mach::Mach::Binary(m)) => m,
                // TODO: smarter way of getting the right version from a fat binary
                Ok(mach::Mach::Fat(m)) => m.get(0).unwrap(),
                _ => {
                    return Err(format_err!("Couldn't parse Mach-O binary"));
                }
            };
            match mach.symbols.as_ref() {
                Some(symbols) => for x in symbols.iter() {
                    let (name, sym) = x.unwrap();
                    if name == symbol_name {
                        return Ok(sym.n_value as usize);
                    }
                },
                None => {}
            };
            Err(format_err!("Couldn't get `{}` symbol. This is likely because rbspy doesn't work with system Ruby on Mac. Try with rbenv/rvm ruby?", symbol_name))
        }
    }

    fn get_ruby_binary(pid: pid_t) -> Result<Binary, Error> {
        let task = task_for_pid(pid).map_err(|_| AddressFinderError::MacPermissionDenied(pid))?;
        let vmmap = get_process_maps(pid, task);
        get_maps_address(&vmmap)
    }

    fn get_maps_address(maps: &Vec<MacMapRange>) -> Result<Binary, Error> {
        let map: &MacMapRange = maps.iter()
            .find(|ref m| {
                if let Some(ref pathname) = m.filename {
                    pathname.contains("bin/ruby") && m.is_exec()
                } else {
                    false
                }
            }).ok_or(format_err!("Couldn't find ruby map"))?;
        Binary::from(map.start as usize, map.filename.as_ref().unwrap())
    }
}

#[cfg(target_os = "linux")]
mod os_impl {
    use copy::*;
    use elf;
    use proc_maps::*;
    use failure::Error;
    use libc::pid_t;
    use std;
    use address_finder::AddressFinderError;
    use read_process_memory::*;

    pub fn current_thread_address(
        pid: pid_t,
        version: &str,
        is_maybe_thread: Box<Fn(usize, &ProcessHandle, &Vec<MapRange>) -> bool>,
    ) -> Result<usize, Error> {
        let proginfo = &get_program_info(pid)?;
        match current_thread_address_symbol_table(proginfo, version) {
            Some(addr) => Ok(addr),
            None => {
                debug!("Trying to find address location another way");
                Ok(current_thread_address_search_bss(
                    proginfo,
                    is_maybe_thread,
                )?)
            }
        }
    }

    pub fn get_ruby_version_address(pid: pid_t) -> Result<usize, Error> {
        let proginfo = &get_program_info(pid)?;
        let ruby_version_symbol = "ruby_version";
        let symbol_addr =
            get_symbol_addr(&proginfo.ruby_map, &proginfo.ruby_elf, ruby_version_symbol);
        match symbol_addr {
            Some(addr) => Ok(addr),
            _ => {
                get_symbol_addr(
                    // if we have a ruby map but `ruby_version` isn't in it, we expect there to be
                    // a libruby map. If that's not true, that's a bug.
                    (*proginfo.libruby_map)
                        .as_ref()
                        .ok_or(format_err!("Missing libruby map. Please report this!"))?,
                    proginfo
                        .libruby_elf
                        .as_ref()
                        .ok_or(format_err!("Missing libruby ELF. Please report this!"))?,
                    ruby_version_symbol,
                ).ok_or(format_err!("Couldn't find ruby version."))
            }
        }
    }

    fn elf_symbol_value(elf_file: &elf::File, symbol_name: &str) -> Option<usize> {
        // TODO: maybe move this to goblin so that it works on OS X & BSD, not just linux
        let sections = &elf_file.sections;
        for s in sections {
            for sym in elf_file
                .get_symbols(&s)
                .expect("Failed to get symbols from section")
            {
                if sym.name == symbol_name {
                    debug!("symbol: {}", sym);
                    return Some(sym.value as usize);
                }
            }
        }
        None
    }

    fn get_bss_section(elf_file: &elf::File) -> Option<elf::types::SectionHeader> {
        for s in &elf_file.sections {
            match s.shdr.name.as_ref() {
                ".bss" => {
                    return Some(s.shdr.clone());
                }
                _ => {}
            }
        }
        None
    }

    fn current_thread_address_search_bss(
        proginfo: &ProgramInfo,
        is_maybe_thread: Box<Fn(usize, &ProcessHandle, &Vec<MapRange>) -> bool>,
    ) -> Result<usize, Error> {
        // Used when there's no symbol table. Looks through the .bss and uses a search_bss (found in
        // `is_maybe_thread`) to find the address of the current thread.
        let map = (*proginfo.libruby_map).as_ref().expect(
            "No libruby map: symbols are stripped so we expected to have one. Please report this!",
        );
        let libruby_elf = proginfo.libruby_elf.as_ref().expect(
            "No libruby elf: symbols are stripped so we expected to have one. Please report this!",
        );
        let bss_section = get_bss_section(libruby_elf).expect(
            "No BSS section (every Ruby ELF file should have a BSS section?). Please report this!",
        );
        let load_header = elf_load_header(libruby_elf);
        debug!("bss_section header: {:?}", bss_section);
        let read_addr = map.range_start + bss_section.addr as usize - load_header.vaddr as usize;

        debug!("read_addr: {:x}", read_addr);
        let source = &proginfo.pid.try_into_process_handle().unwrap();
        let mut data = copy_address_raw(read_addr as usize, bss_section.size as usize, source)?;
        debug!("successfully read data");
        let slice: &[usize] = unsafe {
            std::slice::from_raw_parts(
                data.as_mut_ptr() as *mut usize,
                data.capacity() as usize / std::mem::size_of::<usize>() as usize,
            )
        };

        let i = slice
            .iter()
            .position({ |&x| is_maybe_thread(x, source, &proginfo.all_maps) })
            .ok_or(format_err!(
                "Current thread address not found in process {}",
                &proginfo.pid
            ))?;
        Ok((i as usize) * (std::mem::size_of::<usize>() as usize) + read_addr)
    }

    fn current_thread_address_symbol_table(
        // Uses the symbol table to get the address of the current thread
        proginfo: &ProgramInfo,
        version: &str,
    ) -> Option<usize> {
        // TODO: comment this somewhere
        if version >= "2.5.0" {
            // TODO: make this more robust
            get_symbol_addr(
                &proginfo.ruby_map,
                &proginfo.ruby_elf,
                "ruby_current_execution_context_ptr",
            )
        } else {
            get_symbol_addr(
                &proginfo.ruby_map,
                &proginfo.ruby_elf,
                "ruby_current_thread",
            )
        }
    }

    fn get_symbol_addr(map: &MapRange, elf_file: &elf::File, symbol_name: &str) -> Option<usize> {
        elf_symbol_value(elf_file, symbol_name).map(|addr| {
            let load_header = elf_load_header(elf_file);
            debug!("load header: {}", load_header);
            map.range_start + addr - load_header.vaddr as usize
        })
    }

    fn elf_load_header(elf_file: &elf::File) -> elf::types::ProgramHeader {
        elf_file
            .phdrs
            .iter()
            .find(|ref ph| {
                ph.progtype == elf::types::PT_LOAD && (ph.flags.0 & elf::types::PF_X.0) != 0
            })
            .expect("No executable LOAD header found in ELF file. Please report this!")
            .clone()
    }

    // struct to hold everything we know about the program
    pub struct ProgramInfo {
        pub pid: pid_t,
        pub all_maps: Vec<MapRange>,
        pub ruby_map: Box<MapRange>,
        pub libruby_map: Box<Option<MapRange>>,
        pub ruby_elf: elf::File,
        pub libruby_elf: Option<elf::File>,
    }

    fn open_elf_file(pid: pid_t, map: &MapRange) -> Result<elf::File, Error> {
        // Read binaries from `/proc/PID/root` because the target process might be in a different
        // mount namespace. /proc/PID/root is the view of the filesystem that the target process
        // has. (see the proc man page for more)
        // So we read /usr/bin/ruby from /proc/PID/root/usr/bin/ruby
        let map_path = map.pathname.as_ref().expect("map's pathname shouldn't be None");
        let elf_path = format!("/proc/{}/root{}", pid, map_path);
        elf::File::open_path(&elf_path)
            .map_err(|_| format_err!("Couldn't open ELF file: {:?}", elf_path))
    }

    pub fn get_program_info(pid: pid_t) -> Result<ProgramInfo, Error> {
        let all_maps = get_proc_maps(pid).map_err(|x| match x.kind() {
            std::io::ErrorKind::NotFound => AddressFinderError::NoSuchProcess(pid),
            std::io::ErrorKind::PermissionDenied => AddressFinderError::PermissionDenied(pid),
            _ => AddressFinderError::ProcMapsError(pid),
        })?;
        let ruby_map = Box::new(get_map(&all_maps, "bin/ruby", "r-xp")
            .ok_or(format_err!("Ruby map not found for PID: {}", pid))?);
        let all_maps = get_proc_maps(pid).unwrap();
        let ruby_elf = open_elf_file(pid, &ruby_map)?;
        let libruby_map = Box::new(get_map(&all_maps, "libruby", "r-xp"));
        let libruby_elf = match *libruby_map {
            Some(ref map) => {
                Some(open_elf_file(pid, &map)?)
            }
            _ => None,
        };
        Ok(ProgramInfo {
            pid: pid,
            all_maps: all_maps,
            ruby_map: ruby_map,
            libruby_map: libruby_map,
            ruby_elf: ruby_elf,
            libruby_elf: libruby_elf,
        })
    }

    fn get_map(maps: &Vec<MapRange>, contains: &str, flags: &str) -> Option<MapRange> {
        maps.iter()
            .find(|ref m| {
                if let Some(ref pathname) = m.pathname {
                    pathname.contains(contains) && &m.flags == flags
                } else {
                    false
                }
            })
            .map(|x| x.clone())
    }
}
