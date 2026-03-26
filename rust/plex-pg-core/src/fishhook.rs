use std::mem::size_of;

use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;

#[repr(C)]
pub struct Rebinding {
    pub name: *const c_char,
    pub replacement: *const c_void,
    pub replaced: *mut *mut c_void,
}

#[repr(C)]
struct RebindingsEntry {
    rebindings: *mut Rebinding,
    rebindings_nel: usize,
    next: *mut RebindingsEntry,
}

static mut REBINDINGS_HEAD: *mut RebindingsEntry = ptr::null_mut();

const SEG_DATA: &[u8] = b"__DATA\0";
const SEG_LINKEDIT: &[u8] = b"__LINKEDIT\0";
const SEG_DATA_CONST: &[u8] = b"__DATA_CONST\0";

const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;

const SECTION_TYPE: u32 = 0x000000ff;
const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;
const S_LAZY_SYMBOL_POINTERS: u32 = 0x7;

const INDIRECT_SYMBOL_LOCAL: u32 = 0x80000000;
const INDIRECT_SYMBOL_ABS: u32 = 0x40000000;

const VM_PROT_READ: c_int = 0x01;
const VM_PROT_WRITE: c_int = 0x02;
const VM_PROT_COPY: c_int = 0x10;
const KERN_SUCCESS: c_int = 0;

#[cfg(target_pointer_width = "64")]
type MachHeader = mach_header_64;
#[cfg(target_pointer_width = "64")]
type SegmentCommand = segment_command_64;
#[cfg(target_pointer_width = "64")]
type Section = section_64;
#[cfg(target_pointer_width = "64")]
type Nlist = nlist_64;
#[cfg(target_pointer_width = "64")]
const LC_SEGMENT_ARCH_DEPENDENT: u32 = LC_SEGMENT_64;

#[cfg(target_pointer_width = "32")]
type MachHeader = mach_header;
#[cfg(target_pointer_width = "32")]
type SegmentCommand = segment_command;
#[cfg(target_pointer_width = "32")]
type Section = section;
#[cfg(target_pointer_width = "32")]
type Nlist = nlist;
#[cfg(target_pointer_width = "32")]
const LC_SEGMENT_ARCH_DEPENDENT: u32 = LC_SEGMENT;

#[cfg(target_pointer_width = "32")]
const LC_SEGMENT: u32 = 0x1;

#[repr(C)]
#[cfg(target_pointer_width = "32")]
struct mach_header {
    magic: u32,
    cputype: i32,
    cpusubtype: i32,
    filetype: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
}

#[repr(C)]
struct mach_header_64 {
    magic: u32,
    cputype: i32,
    cpusubtype: i32,
    filetype: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
#[cfg(target_pointer_width = "32")]
struct segment_command {
    cmd: u32,
    cmdsize: u32,
    segname: [c_char; 16],
    vmaddr: u32,
    vmsize: u32,
    fileoff: u32,
    filesize: u32,
    maxprot: i32,
    initprot: i32,
    nsects: u32,
    flags: u32,
}

#[repr(C)]
struct segment_command_64 {
    cmd: u32,
    cmdsize: u32,
    segname: [c_char; 16],
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    maxprot: i32,
    initprot: i32,
    nsects: u32,
    flags: u32,
}

#[repr(C)]
#[cfg(target_pointer_width = "32")]
struct section {
    sectname: [c_char; 16],
    segname: [c_char; 16],
    addr: u32,
    size: u32,
    offset: u32,
    align: u32,
    reloff: u32,
    nreloc: u32,
    flags: u32,
    reserved1: u32,
    reserved2: u32,
}

#[repr(C)]
struct section_64 {
    sectname: [c_char; 16],
    segname: [c_char; 16],
    addr: u64,
    size: u64,
    offset: u32,
    align: u32,
    reloff: u32,
    nreloc: u32,
    flags: u32,
    reserved1: u32,
    reserved2: u32,
    reserved3: u32,
}

#[repr(C)]
#[cfg(target_pointer_width = "32")]
struct nlist {
    n_un: u32,
    n_type: u8,
    n_sect: u8,
    n_desc: i16,
    n_value: u32,
}

#[repr(C)]
struct nlist_64 {
    n_un: u32,
    n_type: u8,
    n_sect: u8,
    n_desc: u16,
    n_value: u64,
}

#[repr(C)]
struct symtab_command {
    cmd: u32,
    cmdsize: u32,
    symoff: u32,
    nsyms: u32,
    stroff: u32,
    strsize: u32,
}

#[repr(C)]
struct dysymtab_command {
    cmd: u32,
    cmdsize: u32,
    ilocalsym: u32,
    nlocalsym: u32,
    iextdefsym: u32,
    nextdefsym: u32,
    iundefsym: u32,
    nundefsym: u32,
    tocoff: u32,
    ntoc: u32,
    modtaboff: u32,
    nmodtab: u32,
    extrefsymoff: u32,
    nextrefsyms: u32,
    indirectsymoff: u32,
    nindirectsyms: u32,
    extreloff: u32,
    nextrel: u32,
    locreloff: u32,
    nlocrel: u32,
}

#[repr(C)]
struct load_command {
    cmd: u32,
    cmdsize: u32,
}

extern "C" {
    fn _dyld_register_func_for_add_image(callback: extern "C" fn(*const MachHeader, isize));
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_header(index: u32) -> *const MachHeader;
    fn _dyld_get_image_vmaddr_slide(index: u32) -> isize;
    fn mach_task_self() -> c_uint;
    fn vm_protect(task: c_uint, address: u64, size: u64, set_max: c_int, prot: c_int) -> c_int;
}

fn segname_eq(segname: &[c_char; 16], name: &[u8]) -> bool {
    let mut i = 0usize;
    while i < segname.len() && i < name.len() {
        let c = segname[i] as u8;
        let n = name[i];
        if c == 0 && n == 0 {
            return true;
        }
        if c != n {
            return false;
        }
        i += 1;
    }
    true
}

unsafe fn prepend_rebindings(
    rebindings_head: *mut *mut RebindingsEntry,
    rebindings: *const Rebinding,
    nel: usize,
) -> c_int {
    let entry = libc::malloc(size_of::<RebindingsEntry>()) as *mut RebindingsEntry;
    if entry.is_null() {
        return -1;
    }
    let reb_mem = libc::malloc(size_of::<Rebinding>() * nel) as *mut Rebinding;
    if reb_mem.is_null() {
        libc::free(entry as *mut c_void);
        return -1;
    }
    ptr::copy_nonoverlapping(rebindings, reb_mem, nel);

    (*entry).rebindings = reb_mem;
    (*entry).rebindings_nel = nel;
    (*entry).next = *rebindings_head;
    *rebindings_head = entry;
    0
}

unsafe fn perform_rebinding_with_section(
    rebindings: *mut RebindingsEntry,
    section: *const Section,
    slide: isize,
    symtab: *const Nlist,
    strtab: *const c_char,
    indirect_symtab: *const u32,
) {
    let indirect_symbol_indices = indirect_symtab.add((*section).reserved1 as usize);
    let indirect_symbol_bindings = (slide + (*section).addr as isize) as *mut *mut c_void;

    let count = (*section).size as usize / size_of::<*mut c_void>();
    for i in 0..count {
        let symtab_index = *indirect_symbol_indices.add(i);
        if symtab_index == INDIRECT_SYMBOL_ABS
            || symtab_index == INDIRECT_SYMBOL_LOCAL
            || symtab_index == (INDIRECT_SYMBOL_LOCAL | INDIRECT_SYMBOL_ABS)
        {
            continue;
        }

        let sym = symtab.add(symtab_index as usize);
        let symbol_name = strtab.add((*sym).n_un as usize);
        if *symbol_name == 0 || *symbol_name.add(1) == 0 {
            continue;
        }

        let mut cur = rebindings;
        while !cur.is_null() {
            for j in 0..(*cur).rebindings_nel {
                let reb = (*cur).rebindings.add(j);
                if libc::strcmp(symbol_name.add(1), (*reb).name) == 0 {
                    if !(*reb).replaced.is_null()
                        && *(*reb).replaced != *indirect_symbol_bindings.add(i)
                    {
                        *(*reb).replaced = *indirect_symbol_bindings.add(i);
                    }

                    let err = vm_protect(
                        mach_task_self(),
                        indirect_symbol_bindings as u64,
                        (*section).size,
                        0,
                        VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY,
                    );
                    if err == KERN_SUCCESS {
                        *indirect_symbol_bindings.add(i) = (*reb).replacement as *mut c_void;
                    }
                    break;
                }
            }
            cur = (*cur).next;
        }
    }
}

unsafe fn rebind_symbols_for_image(
    rebindings: *mut RebindingsEntry,
    header: *const MachHeader,
    slide: isize,
) {
    let mut info = libc::Dl_info {
        dli_fname: ptr::null(),
        dli_fbase: ptr::null_mut(),
        dli_sname: ptr::null(),
        dli_saddr: ptr::null_mut(),
    };
    if libc::dladdr(header as *const c_void, &mut info) == 0 {
        return;
    }

    let header = header as *const MachHeader;
    let mut cur = (header as *const u8).add(size_of::<MachHeader>());
    let mut linkedit_segment: *const SegmentCommand = ptr::null();
    let mut symtab_cmd: *const symtab_command = ptr::null();
    let mut dysymtab_cmd: *const dysymtab_command = ptr::null();

    for _ in 0..(*header).ncmds {
        let lc = &*(cur as *const load_command);
        if lc.cmd == LC_SEGMENT_ARCH_DEPENDENT {
            let seg = &*(cur as *const SegmentCommand);
            if segname_eq(&seg.segname, SEG_LINKEDIT) {
                linkedit_segment = seg as *const SegmentCommand;
            }
        } else if lc.cmd == LC_SYMTAB {
            symtab_cmd = cur as *const symtab_command;
        } else if lc.cmd == LC_DYSYMTAB {
            dysymtab_cmd = cur as *const dysymtab_command;
        }
        cur = cur.add(lc.cmdsize as usize);
    }

    if linkedit_segment.is_null()
        || symtab_cmd.is_null()
        || dysymtab_cmd.is_null()
        || (*dysymtab_cmd).nindirectsyms == 0
    {
        return;
    }

    let linkedit_base = (slide + (*linkedit_segment).vmaddr as isize
        - (*linkedit_segment).fileoff as isize) as usize;
    let symtab = (linkedit_base + (*symtab_cmd).symoff as usize) as *const Nlist;
    let strtab = (linkedit_base + (*symtab_cmd).stroff as usize) as *const c_char;
    let indirect_symtab = (linkedit_base + (*dysymtab_cmd).indirectsymoff as usize) as *const u32;

    cur = (header as *const u8).add(size_of::<MachHeader>());
    for _ in 0..(*header).ncmds {
        let lc = &*(cur as *const load_command);
        if lc.cmd == LC_SEGMENT_ARCH_DEPENDENT {
            let seg = &*(cur as *const SegmentCommand);
            if !segname_eq(&seg.segname, SEG_DATA) && !segname_eq(&seg.segname, SEG_DATA_CONST) {
                cur = cur.add(lc.cmdsize as usize);
                continue;
            }
            let sect_base = cur.add(size_of::<SegmentCommand>()) as *const Section;
            for i in 0..seg.nsects {
                let sect = sect_base.add(i as usize);
                let section_type = (*sect).flags & SECTION_TYPE;
                if section_type == S_LAZY_SYMBOL_POINTERS
                    || section_type == S_NON_LAZY_SYMBOL_POINTERS
                {
                    perform_rebinding_with_section(
                        rebindings,
                        sect,
                        slide,
                        symtab,
                        strtab,
                        indirect_symtab,
                    );
                }
            }
        }
        cur = cur.add(lc.cmdsize as usize);
    }
}

extern "C" fn rebind_symbols_for_image_callback(header: *const MachHeader, slide: isize) {
    unsafe {
        rebind_symbols_for_image(ptr::read(ptr::addr_of!(REBINDINGS_HEAD)), header, slide);
    }
}

#[allow(dead_code)]
unsafe fn rebind_symbols_image(
    header: *const MachHeader,
    slide: isize,
    rebindings: &mut [Rebinding],
) -> c_int {
    let mut local_head: *mut RebindingsEntry = ptr::null_mut();
    let retval = prepend_rebindings(&mut local_head, rebindings.as_ptr(), rebindings.len());
    rebind_symbols_for_image(local_head, header, slide);
    if !local_head.is_null() {
        libc::free((*local_head).rebindings as *mut c_void);
        libc::free(local_head as *mut c_void);
    }
    retval
}

/// # Safety
/// `rebindings` must point to valid, writable rebinding entries for the
/// duration of the call. The caller must ensure the supplied symbol names
/// are null-terminated and remain valid until rebinding completes.
pub unsafe fn rebind_symbols(rebindings: &mut [Rebinding]) -> c_int {
    let retval = prepend_rebindings(
        std::ptr::addr_of_mut!(REBINDINGS_HEAD),
        rebindings.as_ptr(),
        rebindings.len(),
    );
    if retval < 0 {
        return retval;
    }

    let head = ptr::read(ptr::addr_of!(REBINDINGS_HEAD));
    if !head.is_null() && (*head).next.is_null() {
        _dyld_register_func_for_add_image(rebind_symbols_for_image_callback);
    } else {
        let count = _dyld_image_count();
        for i in 0..count {
            let header = _dyld_get_image_header(i);
            let slide = _dyld_get_image_vmaddr_slide(i);
            rebind_symbols_for_image(ptr::read(ptr::addr_of!(REBINDINGS_HEAD)), header, slide);
        }
    }

    retval
}
