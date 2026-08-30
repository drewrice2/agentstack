use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use flate2::{Compression, GzBuilder};
use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header};

const SEED_ROOT: &str = "seed/skills/agentstack";
const ARCHIVE_NAME: &str = "agentstack_seed.tar.gz";
const HASH_NAME: &str = "agentstack_seed.sha256";

fn main() {
    println!("cargo:rerun-if-changed={SEED_ROOT}");
    let archive = build_seed_archive(Path::new(SEED_ROOT)).expect("failed to build seed archive");
    let hash = hex_encode(&Sha256::digest(&archive));

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    fs::write(out_dir.join(ARCHIVE_NAME), archive).expect("failed to write seed archive");
    fs::write(out_dir.join(HASH_NAME), hash).expect("failed to write seed hash");
}

fn build_seed_archive(root: &Path) -> io::Result<Vec<u8>> {
    let mut paths = Vec::new();
    collect_paths(root, root, &mut paths)?;
    paths.sort();

    let gz = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    let mut tar = Builder::new(gz);
    append_dir(&mut tar, Path::new("agentstack"))?;
    for relative in paths {
        let source = root.join(&relative);
        let archive_path = Path::new("agentstack").join(&relative);
        if source.is_dir() {
            append_dir(&mut tar, &archive_path)?;
        } else if source.is_file() {
            append_file(&mut tar, &archive_path, &source)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported seed entry `{}`", source.display()),
            ));
        }
    }
    tar.finish()?;
    let gz = tar.into_inner()?;
    gz.finish()
}

fn collect_paths(root: &Path, dir: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = path
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .to_path_buf();
        paths.push(relative);
        if path.is_dir() {
            collect_paths(root, &path, paths)?;
        }
    }
    Ok(())
}

fn append_dir<W: Write>(tar: &mut Builder<W>, path: &Path) -> io::Result<()> {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    tar.append_data(&mut header, path, io::empty())
}

fn append_file<W: Write>(
    tar: &mut Builder<W>,
    archive_path: &Path,
    source: &Path,
) -> io::Result<()> {
    let bytes = fs::read(source)?;
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    tar.append_data(&mut header, archive_path, bytes.as_slice())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
