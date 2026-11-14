//! A copy of Chrome whose binary zendriver has improved, for
//! [`BrowserBuilder::improve_chrome_binary`](crate::BrowserBuilder::improve_chrome_binary).
//!
//! Each improvement is a [`Patch`]: bytes that replace bytes of Chrome's
//! executable, at a place that zendriver finds in each Chrome build. The one
//! improvement so far is [`headless_user_agent`]'s. Only an x86-64 ELF
//! executable (Linux's Chrome) can be improved so far.
//!
//! The copy is a directory that mirrors Chrome's installation: the files that
//! launching Chrome runs (wrapper scripts, then the executable) are copied,
//! and every other file is a symlink to the original. It is made once per
//! Chrome build, in the user's cache directory.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::error::BrowserError;

/// The version of the set of improvements. Bump it when a patch changes, so
/// that a copy that an older zendriver improved isn't reused.
const IMPROVEMENTS_VERSION: u32 = 1;

/// The most files that launching Chrome may run in turn: wrapper scripts,
/// then the executable.
const MAX_LAUNCH_CHAIN: usize = 4;

/// The 8-byte name that replaces Chrome's user-agent copy of the switch name
/// `headless`: a switch that nobody passes.
const UNSET_SWITCH: &[u8; 8] = b"zd-unset";

/// The furthest, in bytes, that the instruction that loads the user-agent
/// copy of `headless` may be before the one that loads `Headless`: both are
/// in the same function (`GetUserAgentInternal`).
const MAX_SWITCH_TO_PREFIX: u64 = 0x400;

/// A change to Chrome's executable: `bytes` replace as many bytes at
/// `offset`, a position in the file.
#[derive(Debug, PartialEq)]
struct Patch {
    offset: usize,
    bytes: &'static [u8],
}

/// A file that launching Chrome runs.
#[derive(Debug)]
struct Link {
    /// The file's path, with symlinks resolved.
    path: PathBuf,
    /// For a script: the next file's path as the script writes it, if the
    /// copy of the script must name the copy of the next file instead (an
    /// absolute path). `None` for the executable, and for a script that
    /// names the next file relative to its own directory.
    absolute_next: Option<String>,
}

/// A section of an ELF file: its address in memory, and where it is in the
/// file.
#[derive(Debug, Clone, Copy)]
struct Section {
    addr: u64,
    offset: usize,
    size: usize,
}

/// A place that refers to an address.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Reference {
    /// An instruction at this address loads it, relative to the instruction
    /// pointer.
    Code(u64),
    /// A pointer in data holds it.
    Data,
}

/// FNV-1a: a hash that stays the same across Rust versions, unlike
/// `DefaultHasher`'s, so that cache keys do too. `hash` is the hash so far.
fn fnv1a(bytes: &[u8], hash: u64) -> u64 {
    bytes.iter().fold(hash, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// FNV-1a's starting hash.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// The `size`-byte little-endian number at `at` in `bytes`.
fn read_le(bytes: &[u8], at: usize, size: usize) -> Result<u64, String> {
    let field = bytes
        .get(at..at + size)
        .ok_or_else(|| format!("the ELF file ends before byte {}", at + size))?;
    Ok(field
        .iter()
        .rev()
        .fold(0, |value, &byte| (value << 8) | u64::from(byte)))
}

/// The sections named `names` of the 64-bit little-endian ELF file `elf`,
/// in that order.
fn sections<const N: usize>(elf: &[u8], names: [&str; N]) -> Result<[Section; N], String> {
    let to_usize = |value: u64| usize::try_from(value).map_err(|e| e.to_string());
    let table = to_usize(read_le(elf, 0x28, 8)?)?;
    let entry_size = to_usize(read_le(elf, 0x3a, 2)?)?;
    let count = to_usize(read_le(elf, 0x3c, 2)?)?;
    let names_index = to_usize(read_le(elf, 0x3e, 2)?)?;
    let header = |index: usize, field: usize, size: usize| {
        read_le(elf, table + index * entry_size + field, size)
    };
    let names_offset = to_usize(header(names_index, 0x18, 8)?)?;

    let mut found = [None; N];
    for index in 0..count {
        let name_start = names_offset + to_usize(header(index, 0, 4)?)?;
        let name_end = elf
            .get(name_start..)
            .and_then(|rest| rest.iter().position(|&b| b == 0))
            .map(|len| name_start + len)
            .ok_or("a section name doesn't end in the file")?;
        let Some(slot) = names
            .iter()
            .position(|n| n.as_bytes() == &elf[name_start..name_end])
        else {
            continue;
        };
        let section = Section {
            addr: header(index, 0x10, 8)?,
            offset: to_usize(header(index, 0x18, 8)?)?,
            size: to_usize(header(index, 0x20, 8)?)?,
        };
        if section
            .offset
            .checked_add(section.size)
            .is_none_or(|end| end > elf.len())
        {
            return Err(format!("section {} ends after the file", names[slot]));
        }
        found[slot] = Some(section);
    }
    let mut out = [Section {
        addr: 0,
        offset: 0,
        size: 0,
    }; N];
    for (slot, section) in found.into_iter().enumerate() {
        out[slot] = section.ok_or_else(|| format!("no {} section", names[slot]))?;
    }
    Ok(out)
}

/// The addresses in `rodata` where the bytes `text` start, followed by a NUL:
/// a string literal, or the tail of a longer one.
fn string_addresses(elf: &[u8], rodata: Section, text: &[u8]) -> Vec<u64> {
    let bytes = &elf[rodata.offset..rodata.offset + rodata.size];
    let mut needle = text.to_vec();
    needle.push(0);
    bytes
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle.as_slice())
        .map(|(at, _)| rodata.addr + at as u64)
        .collect()
}

/// Every reference to each address in `targets`: from the instructions in
/// `text` that load an address relative to the instruction pointer (a `lea`
/// or `mov` with a RIP-relative operand, with or without a REX prefix), and
/// from the `R_X86_64_RELATIVE` relocations in `rela`, each of which makes a
/// pointer in data. An address that nothing refers to maps to an empty list.
fn references(
    elf: &[u8],
    text: Section,
    rela: Section,
    targets: &[u64],
) -> HashMap<u64, Vec<Reference>> {
    const R_X86_64_RELATIVE: u64 = 8;
    let mut found: HashMap<u64, Vec<Reference>> =
        targets.iter().map(|&t| (t, Vec::new())).collect();

    // Opcode, then a ModRM byte with mod 00 and r/m 101 (RIP-relative), then
    // a 32-bit displacement from the end of the instruction.
    let code = &elf[text.offset..text.offset + text.size];
    for at in 0..code.len().saturating_sub(5) {
        if !matches!(code[at], 0x8b | 0x8d) || code[at + 1] & 0xc7 != 0x05 {
            continue;
        }
        let displacement = i32::from_le_bytes(code[at + 2..at + 6].try_into().expect("4 bytes"));
        let end = text.addr + at as u64 + 6;
        if let Some(list) = found.get_mut(&end.wrapping_add_signed(i64::from(displacement))) {
            list.push(Reference::Code(text.addr + at as u64));
        }
    }

    for entry in elf[rela.offset..rela.offset + rela.size]
        .as_chunks::<24>()
        .0
    {
        let kind = u64::from_le_bytes(entry[8..16].try_into().expect("8 bytes")) & 0xffff_ffff;
        let addend = u64::from_le_bytes(entry[16..24].try_into().expect("8 bytes"));
        if kind == R_X86_64_RELATIVE
            && let Some(list) = found.get_mut(&addend)
        {
            list.push(Reference::Data);
        }
    }
    found
}

/// The patch that takes `Headless` out of Chrome's own user agent.
///
/// When Chrome's command line has `--headless`, it puts `Headless` before
/// `Chrome/` in its user agent (`GetUserAgentInternal` in Chromium's
/// `components/embedder_support/user_agent_utils.cc`). That check reads its
/// own copy of the switch name, `"headless"` (`embedder_support::kHeadless`),
/// which nothing else reads; headless mode itself reads another copy. The
/// patch renames that copy to [`UNSET_SWITCH`], so the check never finds the
/// switch.
///
/// To find the copy: the literal `Headless` has one reference (the code that
/// inserts it), and the copy is the `headless` whose only reference is an
/// instruction at most [`MAX_SWITCH_TO_PREFIX`] bytes before that one. No
/// other string may share the copy's bytes.
fn headless_user_agent(elf: &[u8]) -> Result<Patch, String> {
    let [rodata, text, rela] = sections(elf, [".rodata", ".text", ".rela.dyn"])?;
    if sections(elf, [".relr.dyn"]).is_ok() {
        return Err(
            "the executable packs its relocations (.relr.dyn), which zendriver can't read".into(),
        );
    }
    let prefixes = string_addresses(elf, rodata, b"Headless");
    let copies = string_addresses(elf, rodata, b"headless");
    // A string that shares a copy's bytes starts inside it: tail-merged.
    let insides: Vec<u64> = copies.iter().flat_map(|&copy| copy + 1..copy + 8).collect();
    let targets: Vec<u64> = prefixes
        .iter()
        .chain(&copies)
        .chain(&insides)
        .copied()
        .collect();
    let refs = references(elf, text, rela, &targets);

    let prefix_refs: Vec<Reference> = prefixes
        .iter()
        .flat_map(|p| refs[p].iter().copied())
        .collect();
    let [Reference::Code(inserts_prefix)] = prefix_refs[..] else {
        return Err(format!(
            "want one code reference to the literal Headless, found {prefix_refs:?}"
        ));
    };
    let candidates: Vec<u64> = copies
        .iter()
        .copied()
        .filter(|copy| match refs[copy][..] {
            [Reference::Code(load)] => {
                load < inserts_prefix && inserts_prefix - load <= MAX_SWITCH_TO_PREFIX
            }
            _ => false,
        })
        .collect();
    let [copy] = candidates[..] else {
        return Err(format!(
            "want one copy of \"headless\" loaded just before Headless at {inserts_prefix:#x}, found {}",
            candidates.len()
        ));
    };
    let offset = usize::try_from(copy - rodata.addr).map_err(|e| e.to_string())? + rodata.offset;
    let standalone = elf[offset - 1] == 0;
    if !standalone || (copy + 1..copy + 8).any(|inside| !refs[&inside].is_empty()) {
        return Err(format!(
            "another string shares the bytes of \"headless\" at {copy:#x}"
        ));
    }
    Ok(Patch {
        offset,
        bytes: UNSET_SWITCH,
    })
}

/// The patches that improve the ELF executable `elf`, one per improvement.
fn patches(elf: &[u8]) -> Result<Vec<Patch>, String> {
    const EM_X86_64: u64 = 62;
    if elf.get(..6) != Some(b"\x7fELF\x02\x01") || read_le(elf, 0x12, 2)? != EM_X86_64 {
        return Err("only a 64-bit little-endian x86-64 ELF executable can be improved".into());
    }
    Ok(vec![headless_user_agent(elf)?])
}

/// The path that the last `exec` line of the shell script `script` runs:
/// the first word after `exec` and its options, without quotes. `None` if no
/// `exec` line runs a command (`exec < /dev/null` only redirects).
fn exec_target(script: &str) -> Option<&str> {
    script.lines().rev().find_map(|line| {
        let mut words = line.trim_start().strip_prefix("exec ")?.split_whitespace();
        let mut word = words.next()?;
        while word.starts_with('-') {
            // `-a NAME` sets the command's name; `-c` and `-l` take no value.
            if word == "-a" {
                words.next()?;
            }
            word = words.next()?;
        }
        let word = word.trim_matches(|c| c == '"' || c == '\'');
        let redirects = word.starts_with(['<', '>'])
            || word.trim_start_matches(char::is_numeric).starts_with('>');
        (!word.is_empty() && !redirects).then_some(word)
    })
}

/// The files that launching `executable` runs in turn, ending with Chrome's
/// ELF executable. Each file before it is a script whose last `exec` line
/// runs the next by its absolute path, or by `$HERE/...` (or `${HERE}/...`),
/// relative to the script's own directory (as Chrome's own `google-chrome`
/// script does).
fn launch_chain(executable: &Path) -> Result<Vec<Link>, String> {
    let canonical = |path: &Path| {
        fs::canonicalize(path).map_err(|e| format!("can't resolve {}: {e}", path.display()))
    };
    let mut chain = Vec::new();
    let mut path = canonical(executable)?;
    while chain.len() < MAX_LAUNCH_CHAIN {
        let mut head = [0; 4];
        fs::File::open(&path)
            .and_then(|mut file| file.read_exact(&mut head))
            .map_err(|e| format!("can't read {}: {e}", path.display()))?;
        if head == *b"\x7fELF" {
            chain.push(Link {
                path,
                absolute_next: None,
            });
            return Ok(chain);
        }
        if !head.starts_with(b"#!") {
            return Err(format!(
                "{} is neither an ELF executable nor a script",
                path.display()
            ));
        }
        let script =
            fs::read_to_string(&path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
        let target = exec_target(&script).ok_or_else(|| {
            format!(
                "the script {} has no exec line that runs a file",
                path.display()
            )
        })?;
        let dir = path.parent().expect("a canonical file path has a parent");
        let relative = target
            .strip_prefix("$HERE/")
            .or_else(|| target.strip_prefix("${HERE}/"));
        let (next, absolute_next) = if let Some(rest) = relative {
            (dir.join(rest), None)
        } else if target.starts_with('/') {
            (PathBuf::from(target), Some(target.to_owned()))
        } else {
            return Err(format!(
                "the script {} runs {target}, which zendriver can't resolve",
                path.display()
            ));
        };
        chain.push(Link {
            path,
            absolute_next,
        });
        path = canonical(&next)?;
    }
    Err(format!(
        "launching {} runs more than {MAX_LAUNCH_CHAIN} files",
        executable.display()
    ))
}

/// The deepest directory that contains every file in `chain`.
fn common_root(chain: &[Link]) -> PathBuf {
    let mut root = chain[0]
        .path
        .parent()
        .expect("a canonical file path has a parent")
        .to_path_buf();
    while !chain.iter().all(|link| link.path.starts_with(&root)) {
        root.pop();
    }
    root
}

/// Make `dst` a mirror of the directory `src`: a directory for each one that
/// contains a file of `chain`, a copy of each such file (for the executable,
/// with `patches` applied; for a script, naming the copy of the next file),
/// and a symlink to every other entry. `root` is the directory that `src` is
/// in or equal to, and `mirror_root` is where the mirror of `root` will be
/// once it is made: the copies of scripts name the copies there.
fn mirror(
    src: &Path,
    dst: &Path,
    root: &Path,
    mirror_root: &Path,
    chain: &[Link],
    patches: &[Patch],
) -> std::io::Result<()> {
    fs::create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let from = entry?.path();
        let to = dst.join(from.file_name().expect("a directory entry has a name"));
        let Some(index) = chain.iter().position(|link| link.path == from) else {
            if chain.iter().any(|link| link.path.starts_with(&from)) {
                mirror(&from, &to, root, mirror_root, chain, patches)?;
            } else {
                symlink(&from, &to)?;
            }
            continue;
        };
        if index + 1 == chain.len() {
            // `fs::copy` clones the file where the filesystem can (btrfs, XFS).
            fs::copy(&from, &to)?;
            fs::set_permissions(&to, fs::Permissions::from_mode(0o755))?;
            let mut file = fs::OpenOptions::new().write(true).open(&to)?;
            for patch in patches {
                file.seek(SeekFrom::Start(patch.offset as u64))?;
                file.write_all(patch.bytes)?;
            }
            continue;
        }
        let mut script = fs::read_to_string(&from)?;
        if let Some(next) = &chain[index].absolute_next {
            let copy_of_next =
                mirror_root.join(chain[index + 1].path.strip_prefix(root).expect("in root"));
            script = script.replace(next.as_str(), &copy_of_next.to_string_lossy());
        }
        fs::write(&to, script)?;
        fs::set_permissions(&to, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// The directory that holds zendriver's improved copies of Chrome:
/// `$XDG_CACHE_HOME/zendriver/improved-chrome`, else
/// `$HOME/.cache/zendriver/improved-chrome`.
fn cache_dir() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or("neither XDG_CACHE_HOME nor HOME is set")?;
            PathBuf::from(home).join(".cache")
        }
    };
    Ok(base.join("zendriver").join("improved-chrome"))
}

/// The cache key of the improved copy of `chain`: `{path}-{state}`, where
/// `path` hashes the path of the first file, and `state` hashes the
/// improvements' version and each file's path, size and modification time.
fn cache_key(chain: &[Link]) -> Result<String, String> {
    let path = fnv1a(chain[0].path.as_os_str().as_encoded_bytes(), FNV_OFFSET);
    let mut state = fnv1a(&IMPROVEMENTS_VERSION.to_le_bytes(), FNV_OFFSET);
    for link in chain {
        let meta = fs::metadata(&link.path)
            .map_err(|e| format!("can't stat {}: {e}", link.path.display()))?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos());
        state = fnv1a(link.path.as_os_str().as_encoded_bytes(), state);
        state = fnv1a(&meta.len().to_le_bytes(), state);
        state = fnv1a(&modified.to_le_bytes(), state);
    }
    Ok(format!("{path:016x}-{state:016x}"))
}

/// Make (or reuse) the improved copy of Chrome for `chain`, in `cache`, and
/// return the path to launch: the copy of `chain`'s first file.
fn improved_copy(chain: &[Link], cache: &Path) -> Result<PathBuf, String> {
    let root = common_root(chain);
    let key = cache_key(chain)?;
    let copy = cache.join(&key);
    let launch = copy.join(chain[0].path.strip_prefix(&root).expect("in root"));
    if copy.is_dir() {
        return Ok(launch);
    }

    let executable = &chain.last().expect("a chain has an executable").path;
    let elf =
        fs::read(executable).map_err(|e| format!("can't read {}: {e}", executable.display()))?;
    let patches = patches(&elf)?;
    drop(elf);

    fs::create_dir_all(cache).map_err(|e| format!("can't create {}: {e}", cache.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let building = cache.join(format!(".building-{}-{nanos}", std::process::id()));
    if let Err(e) = mirror(&root, &building, &root, &copy, chain, &patches) {
        let _ = fs::remove_dir_all(&building);
        return Err(format!(
            "can't make the copy in {}: {e}",
            building.display()
        ));
    }
    // Another launch may have made the same copy meanwhile: keep that one.
    if let Err(e) = fs::rename(&building, &copy) {
        let _ = fs::remove_dir_all(&building);
        if !copy.is_dir() {
            return Err(format!("can't move the copy to {}: {e}", copy.display()));
        }
    }
    tracing::info!(copy = %copy.display(), "made an improved copy of Chrome");

    // Delete the copies of older builds at the same path.
    let path_hash = key.split('-').next().expect("a key has a path hash");
    for entry in fs::read_dir(cache).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!("{path_hash}-")) && name != key {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(launch)
}

/// The path to launch in place of `executable`: an improved copy of Chrome
/// (see the module docs), made now or reused from the cache.
///
/// # Errors
/// [`BrowserError::ImproveChromeBinary`] when zendriver can't improve it:
/// launching `executable` doesn't end in an x86-64 ELF executable through
/// scripts that zendriver can follow, a patch's place isn't found exactly
/// once, or making the copy fails.
pub(crate) fn improved_chrome(executable: &Path) -> Result<PathBuf, BrowserError> {
    let copy = launch_chain(executable).and_then(|chain| improved_copy(&chain, &cache_dir()?));
    copy.map_err(|reason| BrowserError::ImproveChromeBinary {
        executable: executable.to_path_buf(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `.rodata` of [`elf`]: the literal `Headless`, then two copies of
    /// the switch name `headless`.
    const RODATA: &[u8] = b"\0Headless\0headless\0headless\0";
    const RODATA_AT: u64 = 0x100;
    const TEXT_AT: u64 = 0x200;
    /// The addresses of [`RODATA`]'s strings.
    const PREFIX: u64 = RODATA_AT + 1;
    const SWITCH: u64 = RODATA_AT + 10;
    const UA_SWITCH: u64 = RODATA_AT + 19;

    /// A minimal x86-64 ELF file whose `.rodata` is [`RODATA`], `.text` is
    /// `code` and `.rela.dyn` is `rela`. Each section's address is its
    /// offset in the file.
    fn elf(code: &[u8], rela: &[u8]) -> Vec<u8> {
        let names = b"\0.rodata\0.text\0.rela.dyn\0.shstrtab\0";
        let mut file = vec![0; TEXT_AT as usize];
        file[..6].copy_from_slice(b"\x7fELF\x02\x01");
        file[0x12..0x14].copy_from_slice(&62u16.to_le_bytes());
        file[RODATA_AT as usize..][..RODATA.len()].copy_from_slice(RODATA);
        file.extend_from_slice(code);
        let rela_at = file.len();
        file.extend_from_slice(rela);
        let names_at = file.len();
        file.extend_from_slice(names);
        let table = file.len();
        // (offset of the name in `names`, address and offset, size)
        let headers = [
            (0, 0, 0),
            (1, RODATA_AT as usize, RODATA.len()),
            (9, TEXT_AT as usize, code.len()),
            (15, rela_at, rela.len()),
            (25, names_at, names.len()),
        ];
        for (name, at, size) in headers {
            let mut header = [0; 64];
            header[..4].copy_from_slice(&(name as u32).to_le_bytes());
            header[0x10..0x18].copy_from_slice(&(at as u64).to_le_bytes());
            header[0x18..0x20].copy_from_slice(&(at as u64).to_le_bytes());
            header[0x20..0x28].copy_from_slice(&(size as u64).to_le_bytes());
            file.extend_from_slice(&header);
        }
        file[0x28..0x30].copy_from_slice(&(table as u64).to_le_bytes());
        file[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        file[0x3c..0x3e].copy_from_slice(&5u16.to_le_bytes());
        file[0x3e..0x40].copy_from_slice(&4u16.to_le_bytes());
        file
    }

    /// `.text` of `nop`s, with a `lea rsi, [rip + ...]` for each
    /// `(at, target)`: an instruction at `TEXT_AT + at` that loads the
    /// address `target`.
    fn code(loads: &[(u64, u64)]) -> Vec<u8> {
        let len = loads
            .iter()
            .map(|&(at, _)| at as usize + 7)
            .max()
            .unwrap_or(0);
        let mut code = vec![0x90; len.max(0x200)];
        for &(at, target) in loads {
            let end = TEXT_AT + at + 7;
            let displacement = i32::try_from(target as i64 - end as i64).expect("near");
            code[at as usize..][..3].copy_from_slice(&[0x48, 0x8d, 0x35]);
            code[at as usize + 3..][..4].copy_from_slice(&displacement.to_le_bytes());
        }
        code
    }

    /// A `.rela.dyn` entry that makes a pointer in data to `target`.
    fn pointer_to(target: u64) -> Vec<u8> {
        [0x1000u64, 8, target]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    }

    /// The loads in Chrome: the headless switch in two places, and its
    /// user-agent copy once, just before `Headless`.
    fn chrome_like_loads() -> Vec<(u64, u64)> {
        vec![
            (0x10, SWITCH),
            (0x20, SWITCH),
            (0x100, UA_SWITCH),
            (0x140, PREFIX),
        ]
    }

    #[test]
    fn finds_the_user_agent_copy_of_the_switch() {
        let file = elf(&code(&chrome_like_loads()), &[]);
        let patch = Patch {
            offset: UA_SWITCH as usize,
            bytes: UNSET_SWITCH,
        };
        assert_eq!(patches(&file), Ok(vec![patch]));
    }

    #[test]
    fn refuses_a_copy_that_data_also_points_to() {
        let file = elf(&code(&chrome_like_loads()), &pointer_to(UA_SWITCH));
        let error = patches(&file).expect_err("two references to the copy");
        assert!(error.contains("want one copy"), "{error}");
    }

    #[test]
    fn refuses_a_second_reference_to_headless() {
        let mut loads = chrome_like_loads();
        loads.push((0x180, PREFIX));
        let error = patches(&elf(&code(&loads), &[])).expect_err("two references to Headless");
        assert!(error.contains("want one code reference"), "{error}");
    }

    #[test]
    fn refuses_a_copy_loaded_too_far_before_headless() {
        let far = 0x100 + MAX_SWITCH_TO_PREFIX + 1;
        let loads = [
            (0x10, SWITCH),
            (0x20, SWITCH),
            (0x100, UA_SWITCH),
            (far, PREFIX),
        ];
        let error = patches(&elf(&code(&loads), &[])).expect_err("too far");
        assert!(error.contains("want one copy"), "{error}");
    }

    #[test]
    fn refuses_another_architecture() {
        let mut file = elf(&code(&chrome_like_loads()), &[]);
        file[0x12] = 183; // EM_AARCH64
        let error = patches(&file).expect_err("not x86-64");
        assert!(error.contains("x86-64"), "{error}");
    }

    #[test]
    fn exec_target_skips_redirections_and_options() {
        let google = "#!/bin/bash\nexec < /dev/null\nexec > >(exec cat)\n\
                      exec 2> >(exec cat >&2)\n\n# Note: exec -a below is a bashism.\n\
                      exec -a \"$0\" \"$HERE/chrome\" \"$@\"\n";
        assert_eq!(exec_target(google), Some("$HERE/chrome"));
        let nixos = "#!/bin/bash -e\nexport A=b\n\
                     exec \"/nix/store/x-chrome/share/google/chrome/google-chrome\"  ${X:+--y} \"$@\" \n";
        assert_eq!(
            exec_target(nixos),
            Some("/nix/store/x-chrome/share/google/chrome/google-chrome")
        );
        assert_eq!(exec_target("#!/bin/sh\nexec < /dev/null\n"), None);
    }

    /// Write `contents` to `path` as an executable file.
    fn executable(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        fs::write(path, contents).expect("write");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// An installation like NixOS's Chrome: a wrapper that runs Chrome's own
    /// script by its absolute path, which runs the executable next to it.
    /// The copy runs the copies, patches the executable and links the rest;
    /// a second launch reuses it, and a changed executable replaces it.
    #[test]
    fn mirrors_a_wrapped_installation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package = fs::canonicalize(dir.path())
            .expect("canonical")
            .join("package");
        let inner = package.join("share/chrome/google-chrome");
        let wrapper = format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", inner.display());
        executable(
            &package.join("bin/google-chrome-stable"),
            wrapper.as_bytes(),
        );
        executable(
            &inner,
            b"#!/bin/sh\nexec -a \"$0\" \"$HERE/chrome\" \"$@\"\n",
        );
        executable(
            &package.join("share/chrome/chrome"),
            &elf(&code(&chrome_like_loads()), &[]),
        );
        fs::write(package.join("share/chrome/resources.pak"), b"pak").expect("write");
        let cache = dir.path().join("cache");

        let chain = launch_chain(&package.join("bin/google-chrome-stable")).expect("chain");
        let launch = improved_copy(&chain, &cache).expect("copy");
        let copy = launch
            .parent()
            .and_then(Path::parent)
            .expect("the copy")
            .to_path_buf();
        assert_eq!(launch, copy.join("bin/google-chrome-stable"));
        let copied_wrapper = fs::read_to_string(&launch).expect("read");
        let copy_of_inner = copy.join("share/chrome/google-chrome");
        assert!(
            copied_wrapper.contains(&*copy_of_inner.to_string_lossy()),
            "{copied_wrapper}"
        );
        assert!(
            !fs::symlink_metadata(&copy_of_inner)
                .expect("stat")
                .is_symlink()
        );
        let patched = fs::read(copy.join("share/chrome/chrome")).expect("read");
        assert_eq!(&patched[UA_SWITCH as usize..][..8], UNSET_SWITCH);
        let pak = copy.join("share/chrome/resources.pak");
        assert_eq!(
            fs::read_link(&pak).expect("a symlink"),
            package.join("share/chrome/resources.pak")
        );

        assert_eq!(improved_copy(&chain, &cache).expect("reused"), launch);

        let mut changed = elf(&code(&chrome_like_loads()), &[]);
        changed.push(0);
        executable(&package.join("share/chrome/chrome"), &changed);
        let chain = launch_chain(&package.join("bin/google-chrome-stable")).expect("chain");
        let relaunch = improved_copy(&chain, &cache).expect("new copy");
        assert_ne!(relaunch, launch);
        assert!(!copy.exists(), "the copy of the older build is deleted");
    }
}
