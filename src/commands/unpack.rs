use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::output::Ctx;
use crate::package::unpack_package;

pub struct Args {
    pub archive: PathBuf,
    pub out: PathBuf,
    pub force: bool,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let unpacked = unpack_package(&args.archive, &args.out, args.force)
        .with_context(|| format!("failed to unpack `{}`", args.archive.display()))?;

    if ctx.json {
        let payload = UnpackJson {
            name: &unpacked.manifest.name,
            out: &unpacked.out_path,
            files: unpacked.manifest.files.len(),
            sha256: &unpacked.hash.hex,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say(format!("unpacked {}", unpacked.out_path.display()));
    ctx.say(format!("  name:        {}", unpacked.manifest.name));
    ctx.say(format!("  files:       {}", unpacked.manifest.files.len()));
    ctx.say(format!("  sha256:      {}", unpacked.hash.hex));
    ctx.say("");
    ctx.say("next:");
    ctx.say(format!(
        "  agentstack skill validate {}",
        unpacked.out_path.display()
    ));
    ctx.say(format!(
        "  agentstack skill inspect {}",
        unpacked.out_path.display()
    ));
    Ok(())
}

#[derive(Serialize)]
struct UnpackJson<'a> {
    name: &'a str,
    out: &'a std::path::Path,
    files: usize,
    sha256: &'a str,
}
