mod app;
mod launch;
mod search;
mod types;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 620.0])
            .with_min_inner_size([1100.0, 620.0])
            .with_decorations(false)
            .with_resizable(false)
            .with_active(true)
            .with_title("nixos-package-search")
            .with_app_id("nixos-package-search"),
        ..Default::default()
    };

    eframe::run_native(
        "nixos-package-search",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(app::App::new()))
        }),
    )
}
