//! The probe's tray load: one StatusNotifierItem with a fat static
//! pixmap, registered with the shell's own watcher. The shell re-reads
//! item properties on its 2s poll, so whatever the shell does per read
//! (re-convert a fresh image, or pin by content hash) is driven here,
//! deterministically, in an environment that otherwise has no tray.
//!
//! Not installed, not part of the shell's runtime: a probe fixture.

use zbus::connection::Builder;

/// The pixmap: big enough that per-poll re-mints read cleanly in the
/// probe's tray phase (512x512 ARGB32 is 1 MiB a mint).
const PIX: i32 = 512;

struct FakeItem;

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl FakeItem {
    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".to_string()
    }

    #[zbus(property)]
    fn id(&self) -> String {
        "kuma-probe-tray".to_string()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        "probe tray load".to_string()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "Active".to_string()
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> (i32, i32, Vec<u8>) {
        static PIXMAP: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        let data = PIXMAP.get_or_init(|| {
            (0..(PIX * PIX * 4) as usize)
                .map(|i| (i as u8) ^ 0xA5)
                .collect()
        });
        (PIX, PIX, data.clone())
    }
}

fn main() {
    if let Err(err) = smol::block_on(run()) {
        eprintln!("fake tray failed: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> zbus::Result<()> {
    let connection = Builder::session()?
        .serve_at("/StatusNotifierItem", FakeItem)?
        .build()
        .await?;
    // the shell's watcher takes the caller's unique name when the
    // service starts with ':'
    let service = connection
        .unique_name()
        .map(|name| name.to_string())
        .unwrap_or_else(|| ":probe".to_string());
    let watcher = zbus::Proxy::new(
        &connection,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )
    .await?;
    watcher
        .call_noreply("RegisterStatusNotifierItem", &(&service,))
        .await?;
    eprintln!("fake tray registered as {service}");
    std::future::pending::<()>().await;
    unreachable!()
}
