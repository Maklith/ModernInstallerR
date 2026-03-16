use eframe::egui::{self, FontFamily};

const HARMONY_FONT: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/HarmonyOS_Sans_SC_Subset.ttf"));

pub fn apply_harmony_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "harmony".to_owned(),
        egui::FontData::from_static(HARMONY_FONT).into(),
    );

    if let Some(proportional) = fonts.families.get_mut(&FontFamily::Proportional) {
        proportional.insert(0, "harmony".to_owned());
    }
    if let Some(monospace) = fonts.families.get_mut(&FontFamily::Monospace) {
        monospace.push("harmony".to_owned());
    }
    fonts
        .families
        .entry(FontFamily::Name("harmony".into()))
        .or_default()
        .push("harmony".to_owned());

    ctx.set_fonts(fonts);
}
