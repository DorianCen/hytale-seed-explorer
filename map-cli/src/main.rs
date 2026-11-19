//! `map-cli` — the headless command line of the Hytale seed explorer.
//!
//! Subcommands: `render` (T11, PNG region renders), `info`. `fidelity`
//! (T05) and `bench` (T04) arrive with their tasks. Everything delegates
//! to `map-core`; no GUI dependency exists here.

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};
use map_core::render::{Layer, encode_png, render_biome_rgba};
use map_core::source::WorldSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig, default_world_path};
use map_core::types::Rect;

#[derive(Parser)]
#[command(
    name = "map-cli",
    version,
    about = "Headless Hytale seed explorer: render maps and search seeds without a GUI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the generator identity this build resolves against.
    Info,

    /// Render a region to a PNG (T11: biome layer, one pixel per sample).
    Render(RenderArgs),
}

#[derive(clap::Args)]
struct RenderArgs {
    /// The seed exactly as the user would type it (numeric, T02 semantics).
    #[arg(long)]
    seed: String,

    /// Region center in block coordinates, `x,z` (negatives allowed).
    #[arg(long, allow_hyphen_values = true, default_value = "0,0")]
    center: String,

    /// Samples per axis; the PNG is `size` x `size` pixels.
    #[arg(long, default_value_t = 512)]
    size: u32,

    /// Blocks per sample (a power of two: 1, 2, 4, ...).
    #[arg(long, default_value_t = 2)]
    step: u32,

    /// The layer to render: `biome`, `height`, `relief`, `fluid` and `zone`
    /// render (T13/T17/T19); the rest arrive with T20/T28.
    #[arg(long, default_value = "biome")]
    layer: Layer,

    /// Output PNG path.
    #[arg(long, default_value = "target/region.png")]
    out: PathBuf,

    /// World-gen folder (Java `HytaleWorldGenProvider` resolution, like
    /// `worldgen-cli --world`); defaults to the extracted pack when
    /// present, else the golden world copy of the sibling checkout.
    #[arg(long, default_value_os_t = default_world_path())]
    world: PathBuf,

    /// Extracted base-game asset root, when the world needs one.
    #[arg(long)]
    assets: Option<PathBuf>,

    /// A `registries.json` snapshot path override.
    #[arg(long)]
    registries: Option<PathBuf>,

    /// Worker pool size (default 2; each worker loads its own generator).
    #[arg(long)]
    workers: Option<usize>,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Info => info(),
        Command::Render(args) => render(args),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn info() -> Result<(), String> {
    println!(
        "generator_version {} (map-core {}, headless)",
        map_core::GENERATOR_VERSION,
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

/// Parses `x,z` block coordinates, allowing negatives on both axes.
fn parse_center(text: &str) -> Result<(i32, i32), String> {
    let (x, z) = text
        .split_once(',')
        .ok_or_else(|| format!("--center must be \"x,z\" with integer values, got {text:?}"))?;
    let x = x
        .trim()
        .parse()
        .map_err(|error| format!("--center x is not an i32 ({x:?}): {error}"))?;
    let z = z
        .trim()
        .parse()
        .map_err(|error| format!("--center z is not an i32 ({z:?}): {error}"))?;
    Ok((x, z))
}

/// Computes the block rect behind a render: `size` samples per axis at
/// `step` blocks each, centered on `center`, i.e. `size * step` blocks
/// per side. `i64` throughout so absurd `--size`/`--step` values fail
/// cleanly instead of wrapping.
fn center_rect(center: (i32, i32), size: u32, step: u32) -> Result<Rect, String> {
    if !step.is_power_of_two() {
        return Err(format!("--step must be a power of two, got {step}"));
    }
    let blocks_per_side = i64::from(size)
        .checked_mul(i64::from(step))
        .ok_or_else(|| format!("--size {size} times --step {step} overflows"))?;
    let half = blocks_per_side / 2;
    let min_x = i64::from(center.0) - half;
    let min_z = i64::from(center.1) - half;
    let max_x = min_x + blocks_per_side - 1;
    let max_z = min_z + blocks_per_side - 1;
    let to_i32 = |value: i64, axis: &str| {
        i32::try_from(value).map_err(|_| {
            format!(
                "--center ({}, {}) at size {size} step {step} puts {axis} outside i32",
                center.0, center.1
            )
        })
    };
    Ok(Rect::new(
        to_i32(min_x, "x")?,
        to_i32(min_z, "z")?,
        to_i32(max_x, "x")?,
        to_i32(max_z, "z")?,
    ))
}

fn render(args: RenderArgs) -> Result<(), String> {
    // Fail on the cheap arguments before touching the generator: seed and
    // center are parsed up front, so a typo costs nothing (the source
    // re-validates the seed; this is fail-fast UX, not a second semantics).
    map_core::types::Seed::parse(&args.seed)
        .map_err(|error| format!("invalid seed {:?}: {error}", args.seed))?;
    let center = parse_center(&args.center)?;
    let rect = center_rect(center, args.size, args.step)?;

    let mut config = WorldgenSourceConfig::new(&args.world, &args.seed);
    config.assets_root = args.assets;
    config.registries_path = args.registries;
    config.workers = args.workers;

    eprintln!("loading world {}", args.world.display());
    let started = Instant::now();
    let source = WorldgenSource::new(&config).map_err(|error| error.to_string())?;
    let loaded = started.elapsed();

    eprintln!(
        "rendering {}x{} @ step {} ({}x{} blocks around {}, {}) via {}",
        args.size,
        args.size,
        args.step,
        args.size * args.step,
        args.size * args.step,
        center.0,
        center.1,
        source.worker_count(),
    );
    let cancel = map_core::scheduler::cancel_token();
    let started = Instant::now();
    // The layer picks the query and the coloring (T17/T19): heights ride
    // the cheap per-column path; fluids ride the chunk-summary surface
    // path; zones ride the cheap per-sample zone path; relief expands the
    // rect by one sample ring so the output's edges shade exactly like the
    // interior (the same reason tiles carry a border). The renderer-less
    // layers name their pending task.
    let image = match args.layer {
        Layer::Biome => {
            let grid = source
                .biome_region(rect, args.step)
                .map_err(|error| error.to_string())?;
            render_biome_rgba(&grid, source.biome_catalog())
        }
        Layer::Height => {
            let grid = source
                .height_region(rect, args.step)
                .map_err(|error| error.to_string())?;
            map_core::render::height::height_image(&grid, &cancel)
                .map_err(|error| error.to_string())?
        }
        Layer::Relief => {
            let bordered = expand_by_step(rect, args.step)?;
            let grid = source
                .height_region(bordered, args.step)
                .map_err(|error| error.to_string())?;
            // The grid starts one sample north-west of the output; its
            // outer ring is read for shading, never painted.
            map_core::render::relief::relief_image(&grid, (-1, -1), args.size, args.size, &cancel)
                .map_err(|error| error.to_string())?
        }
        Layer::Fluid => {
            let grid = source
                .surface_region(rect, args.step)
                .map_err(|error| error.to_string())?;
            map_core::render::fluid::fluid_image(
                &grid,
                source.biome_catalog(),
                map_core::render::fluid::DEFAULT_FLUID_OPACITY,
                &cancel,
            )
            .map_err(|error| error.to_string())?
        }
        Layer::Zone => {
            let grid = source
                .zone_region(rect, args.step)
                .map_err(|error| error.to_string())?;
            map_core::render::zone::zone_image(&grid, source.zone_catalog(), &cancel)
                .map_err(|error| error.to_string())?
        }
        Layer::Surface | Layer::CaveSlice | Layer::CaveDensity => {
            return Err(format!(
                "the layer '{}' has no renderer yet (T20/T28 wire them)",
                args.layer
            ));
        }
    };
    let sampled = started.elapsed();
    let bytes = encode_png(&image).map_err(|error| error.to_string())?;

    write_png(&args.out, &bytes)?;
    println!(
        "wrote {} ({}x{} px, step {}, {}x{} blocks, layer {}, load {:.2}s, sample+render {:.2}s)",
        args.out.display(),
        args.size,
        args.size,
        args.step,
        args.size * args.step,
        args.size * args.step,
        args.layer,
        loaded.as_secs_f64(),
        sampled.as_secs_f64(),
    );
    Ok(())
}

/// Expands `rect` by one `step`-sample ring on every side — relief's
/// neighbour heights. `i64` throughout so absurd sizes fail cleanly
/// instead of wrapping.
fn expand_by_step(rect: Rect, step: u32) -> Result<Rect, String> {
    let span = i64::from(step);
    let min_x = i64::from(rect.min_x)
        .checked_sub(span)
        .ok_or_else(|| format!("--center {} at step {step} underflows x", rect.min_x))?;
    let min_z = i64::from(rect.min_z)
        .checked_sub(span)
        .ok_or_else(|| format!("--center {} at step {step} underflows z", rect.min_z))?;
    let max_x = i64::from(rect.max_x)
        .checked_add(span)
        .ok_or_else(|| format!("--center {} at step {step} overflows x", rect.max_x))?;
    let max_z = i64::from(rect.max_z)
        .checked_add(span)
        .ok_or_else(|| format!("--center {} at step {step} overflows z", rect.max_z))?;
    let to_i32 = |value: i64| {
        i32::try_from(value)
            .map_err(|_| format!("the relief border at step {step} leaves the i32 block space"))
    };
    Ok(Rect::new(
        to_i32(min_x)?,
        to_i32(min_z)?,
        to_i32(max_x)?,
        to_i32(max_z)?,
    ))
}

/// Writes the encoded PNG bytes, creating parent directories so
/// `--out target/nested/dir/region.png` works.
fn write_png(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|error| format!("writing {}: {error}", path.display()))
}
