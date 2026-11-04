use crate::types::PackageHit;
use std::fs;
use std::path::PathBuf;

/// Where search.nixos.org serves the icons its index references — the same
/// URL its own frontend builds (`"/icons/" ++ file`). Since schema 52 the
/// index carries a `package_desktop_icons` map of
/// `{ icon name: "<md5>.<png|svg>" }` per package, which is the only reason
/// icons are possible here at all: a package's icon otherwise exists only
/// inside its (unbuilt) store path.
const ICON_BASE: &str = "https://search.nixos.org/icons/";

/// Icons are drawn small; this is the size SVGs get rasterized to, at 2x the
/// on-screen size so they stay crisp. PNGs are decoded at whatever size they
/// ship and scaled when drawn.
const RENDER_PX: u32 = 64;

/// A decoded icon, in straight (non-premultiplied) RGBA — what
/// `egui::ColorImage::from_rgba_unmultiplied` wants. Kept free of egui types
/// so decoding can happen on a worker thread that knows nothing about the UI.
pub struct Rgba {
    pub size: [usize; 2],
    pub pixels: Vec<u8>,
}

/// Which icon file to use for `hit`, if the index has one.
///
/// A package can ship several desktop entries (`vlc` has five), so prefer the
/// icon whose name matches what the package is actually called before
/// falling back to any of them — and make that fallback deterministic, since
/// a map's iteration order isn't.
pub fn icon_file(hit: &PackageHit) -> Option<&str> {
    let preferred = [
        hit.main_program.as_deref().unwrap_or_default(),
        hit.pname.as_str(),
        hit.attr_name.as_str(),
    ];
    for name in preferred {
        if let Some(file) = hit.desktop_icons.get(name) {
            return Some(file);
        }
    }
    hit.desktop_icons
        .iter()
        .min_by_key(|(name, _)| name.as_str())
        .map(|(_, file)| file.as_str())
}

/// `$XDG_CACHE_HOME/nixos-package-search/icons/<file>`, i.e.
/// `~/.cache/...` by default. Cache rather than state (where the history
/// itself lives): every one of these can be fetched again from
/// search.nixos.org, so losing the lot costs a redownload and nothing else.
fn cache_path(file: &str) -> Option<PathBuf> {
    // `file` comes from the index as `<md5>.<ext>`. It still goes through a
    // check before being used as a path, because it arrives over the network
    // and is about to be joined onto a directory: anything with a separator
    // or a parent-dir hop in it is refused rather than sanitized.
    if file.is_empty()
        || file.len() > 64
        || !file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        || file.contains("..")
    {
        return None;
    }
    let cache_home = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(
        cache_home
            .join("nixos-package-search")
            .join("icons")
            .join(file),
    )
}

/// The icon's bytes: from the cache if they're there, otherwise fetched once
/// and written to it. A failed write is not a failed load — the icon still
/// gets drawn this run, it just gets fetched again next time.
fn bytes(client: &reqwest::blocking::Client, file: &str) -> Option<Vec<u8>> {
    let path = cache_path(file)?;
    if let Ok(cached) = fs::read(&path) {
        if !cached.is_empty() {
            return Some(cached);
        }
    }

    let resp = client.get(format!("{ICON_BASE}{file}")).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let downloaded = resp.bytes().ok()?.to_vec();
    if downloaded.is_empty() {
        return None;
    }

    if let Some(dir) = path.parent() {
        if fs::create_dir_all(dir).is_ok() {
            let tmp = path.with_extension("part");
            if fs::write(&tmp, &downloaded).is_ok() && fs::rename(&tmp, &path).is_err() {
                let _ = fs::remove_file(&tmp);
            }
        }
    }
    Some(downloaded)
}

/// Fetches (or reads from cache) and decodes one icon. Returns `None` for
/// anything that doesn't work out — a 404, a format we don't decode, a
/// corrupt file: a missing icon is a blank space in a list, never an error
/// worth showing anybody.
pub fn load(client: &reqwest::blocking::Client, file: &str) -> Option<Rgba> {
    let data = bytes(client, file)?;
    let is_svg = file
        .rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg") || ext.eq_ignore_ascii_case("svgz"));
    if is_svg {
        decode_svg(&data)
    } else {
        decode_png(&data)
    }
}

fn decode_png(data: &[u8]) -> Option<Rgba> {
    let decoded = image::load_from_memory_with_format(data, image::ImageFormat::Png).ok()?;
    let rgba = decoded.to_rgba8();
    Some(Rgba {
        size: [rgba.width() as usize, rgba.height() as usize],
        pixels: rgba.into_raw(),
    })
}

/// resvg is built without its text/font features: an application icon is
/// shapes, and pulling in a font stack (fontdb, rustybuzz, and the font
/// loading they do at startup) to render text that isn't there would cost
/// far more than it's worth. An icon that does contain text loses the text,
/// not the icon.
fn decode_svg(data: &[u8]) -> Option<Rgba> {
    let tree = resvg::usvg::Tree::from_data(data, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    if longest <= 0.0 {
        return None;
    }
    let scale = RENDER_PX as f32 / longest;
    let width = (size.width() * scale).round().max(1.0) as u32;
    let height = (size.height() * scale).round().max(1.0) as u32;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny-skia renders premultiplied; egui's `from_rgba_unmultiplied` wants
    // it straight, and feeding one to the other silently darkens every
    // antialiased edge.
    let pixels = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Some(Rgba {
        size: [width as usize, height as usize],
        pixels,
    })
}
