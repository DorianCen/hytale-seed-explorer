//! The left-panel search form and results table (T26).
//!
//! This module is the egui skin over [`crate::search_state`]: it builds a
//! [`SearchRequest`] from the form, starts/cancels the headless state, renders
//! the streamed rows with a virtualized scroll area, and translates a row
//! click into the map actions (center, flash, copy). All engine and ordering
//! behavior lives in the headless module; the panel only reads it.
//!
//! Column layout per row: a category-colored icon, the name, the category
//! (zone name for cave entrances), X/Y/Z coordinates and the distance. Rows
//! are ordered by the state, never re-sorted here.

use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use egui::{Color32, RichText};

use map_core::features::category_for;
use map_core::search::feature::{DEFAULT_REGION_SIZE, ProximityFilter};
use map_core::source::FeatureSource;
use map_core::source::WorldSource;
use map_core::types::{
    BiomeId, Feature, FeatureFilter, FeatureKind, FeatureKindMask, FeatureMetadata,
};

use crate::inspector::parse_go_to;
use crate::labels;
use crate::search_state::{
    COARSE_STEPS, DEFAULT_MAX_RESULTS, HitKind, RADIUS_CHOICES, SearchHit, SearchRequest,
    SearchSource, SearchStatus, SearchTarget, format_coordinates, format_distance,
    format_patch_area,
};
use crate::state::AppState;

/// One biome choice: id, display name and map color.
type BiomeChoice = (BiomeId, String, [u8; 3]);

/// The run-button height, shared by the results row height so scrolling is
/// predictable.
const ROW_HEIGHT: f32 = 20.0;

/// The feature-kind selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeatureKindChoice {
    All,
    UniquePrefab,
    BiomePrefab,
    Spawn,
    Poi,
    CaveEntrance,
}

impl FeatureKindChoice {
    const ALL: [FeatureKindChoice; 6] = [
        FeatureKindChoice::All,
        FeatureKindChoice::UniquePrefab,
        FeatureKindChoice::BiomePrefab,
        FeatureKindChoice::Spawn,
        FeatureKindChoice::Poi,
        FeatureKindChoice::CaveEntrance,
    ];

    fn label(self) -> &'static str {
        match self {
            FeatureKindChoice::All => labels::SEARCH_KIND_ALL,
            FeatureKindChoice::UniquePrefab => labels::SEARCH_KIND_UNIQUE,
            FeatureKindChoice::BiomePrefab => labels::SEARCH_KIND_BIOME_PREFAB,
            FeatureKindChoice::Spawn => labels::SEARCH_KIND_SPAWN,
            FeatureKindChoice::Poi => labels::SEARCH_KIND_POI,
            FeatureKindChoice::CaveEntrance => labels::SEARCH_KIND_CAVE,
        }
    }

    fn mask(self) -> FeatureKindMask {
        match self {
            // "All kinds" deliberately excludes cave entrances: the Default
            // world's entrance scan is dense enough to dominate a general
            // search, and cave entrances have their own kind choice (and the
            // T27 overlay). This keeps the general search on 512-block
            // regions.
            FeatureKindChoice::All => FeatureKindMask::from_kind(FeatureKind::UniquePrefab)
                .union(FeatureKindMask::from_kind(FeatureKind::BiomePrefab))
                .union(FeatureKindMask::from_kind(FeatureKind::Spawn))
                .union(FeatureKindMask::from_kind(FeatureKind::Poi)),
            FeatureKindChoice::UniquePrefab => {
                FeatureKindMask::from_kind(FeatureKind::UniquePrefab)
            }
            FeatureKindChoice::BiomePrefab => FeatureKindMask::from_kind(FeatureKind::BiomePrefab),
            FeatureKindChoice::Spawn => FeatureKindMask::from_kind(FeatureKind::Spawn),
            FeatureKindChoice::Poi => FeatureKindMask::from_kind(FeatureKind::Poi),
            FeatureKindChoice::CaveEntrance => {
                FeatureKindMask::from_kind(FeatureKind::CaveEntrance)
            }
        }
    }
}

/// Where the request's center comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CenterMode {
    View,
    Custom,
}

/// The persistent form state (session state, like the layer stack).
pub struct SearchPanel {
    mode: SearchMode,
    kind: FeatureKindChoice,
    category: String,
    name_contains: String,
    min_size_enabled: bool,
    min_size: i32,
    proximity_enabled: bool,
    proximity: i32,
    biome: Option<BiomeId>,
    min_patch_enabled: bool,
    min_patch: u32,
    coarse_step: u32,
    center_mode: CenterMode,
    custom_x: String,
    custom_z: String,
    radius_index: usize,
    max_results: usize,
    /// A form-validation error (the state's engine error is shown beside the
    /// progress line separately).
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Features,
    Biome,
}

impl Default for SearchPanel {
    fn default() -> Self {
        Self {
            mode: SearchMode::Features,
            kind: FeatureKindChoice::All,
            category: String::new(),
            name_contains: String::new(),
            min_size_enabled: false,
            min_size: 1,
            proximity_enabled: false,
            proximity: 64,
            biome: None,
            min_patch_enabled: false,
            min_patch: 4_096,
            coarse_step: map_core::search::biome::DEFAULT_COARSE_STEP,
            center_mode: CenterMode::View,
            custom_x: String::new(),
            custom_z: String::new(),
            radius_index: 2,
            max_results: DEFAULT_MAX_RESULTS,
            error: None,
        }
    }
}

impl SearchPanel {
    /// Draws the panel's body inside the left panel. `focus` is the `F`
    /// shortcut's one-shot: it requests keyboard focus on the Search button
    /// and paints the heading highlighted.
    pub fn ui(&mut self, ui: &mut egui::Ui, state: &mut AppState, focus: bool) {
        let biomes = self.biome_choices(state);
        let categories = self.category_choices(state);
        let has_source = state.source().is_some();
        let view_center = state.view().center();
        let heading = if focus {
            RichText::new(labels::GROUP_SEARCH)
                .strong()
                .color(Color32::from_rgb(120, 200, 255))
        } else {
            RichText::new(labels::GROUP_SEARCH).strong()
        };
        let mut header = egui::CollapsingHeader::new(heading).default_open(true);
        if focus {
            // `F` opens the section even if the user collapsed it.
            header = header.open(Some(true));
        }
        let response = header.show(ui, |ui| {
            self.body(
                ui,
                state,
                &biomes,
                &categories,
                has_source,
                view_center,
                focus,
            );
        });
        // Closing the section while a search runs cancels it (the task's
        // "closing the panel cancels" rule). The header toggles itself in
        // egui's memory; a click while running can only mean closing.
        if response.header_response.clicked() && state.search().is_running() {
            state.search_mut().cancel();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn body(
        &mut self,
        ui: &mut egui::Ui,
        state: &mut AppState,
        biomes: &[BiomeChoice],
        categories: &[(String, String)],
        has_source: bool,
        view_center: map_core::coords::WorldPos,
        focus: bool,
    ) {
        if !has_source {
            ui.label(RichText::new(labels::SEARCH_NEED_SOURCE).weak());
            return;
        }

        self.mode_row(ui);
        self.center_row(ui, view_center);
        match self.mode {
            SearchMode::Features => self.feature_filters(ui, categories),
            SearchMode::Biome => self.biome_filters(ui, biomes),
        }
        self.limits_row(ui);

        let running = state.search().is_running();
        let mut submitted = false;
        let mut cancelled = false;
        let mut cleared = false;
        ui.horizontal(|ui| {
            let go = ui.add_enabled(!running, egui::Button::new(labels::SEARCH_GO));
            if focus {
                go.request_focus();
            }
            if go.clicked() {
                submitted = true;
            }
            if ui
                .add_enabled(running, egui::Button::new(labels::SEARCH_CANCEL))
                .clicked()
            {
                cancelled = true;
            }
            let has_results = !state.search().results().is_empty();
            if ui
                .add_enabled(
                    !running && has_results,
                    egui::Button::new(labels::SEARCH_CLEAR),
                )
                .clicked()
            {
                cleared = true;
            }
        });
        if submitted {
            match self.build_request(state) {
                Ok(request) => {
                    self.error = None;
                    self.start_request(state, request);
                }
                Err(message) => self.error = Some(message),
            }
        }
        if cancelled {
            state.search_mut().cancel();
        }
        if cleared {
            state.search_mut().reset();
            self.error = None;
            state.set_status(labels::STATUS_SEARCH_CLEARED);
        }

        self.status_and_progress(ui, state);
        self.results_table(ui, state, biomes);
    }

    fn mode_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(labels::SEARCH_MODE);
            let current = self.mode;
            let mut selected = current;
            egui::ComboBox::from_id_salt("search-mode")
                .selected_text(match current {
                    SearchMode::Features => labels::SEARCH_MODE_FEATURES,
                    SearchMode::Biome => labels::SEARCH_MODE_BIOME,
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut selected,
                        SearchMode::Features,
                        labels::SEARCH_MODE_FEATURES,
                    );
                    ui.selectable_value(
                        &mut selected,
                        SearchMode::Biome,
                        labels::SEARCH_MODE_BIOME,
                    );
                });
            self.mode = selected;
        });
    }

    fn center_row(&mut self, ui: &mut egui::Ui, view_center: map_core::coords::WorldPos) {
        ui.horizontal(|ui| {
            ui.label(labels::SEARCH_CENTER);
            ui.radio_value(
                &mut self.center_mode,
                CenterMode::View,
                labels::SEARCH_CENTER_VIEW,
            );
            ui.radio_value(
                &mut self.center_mode,
                CenterMode::Custom,
                labels::SEARCH_CENTER_CUSTOM,
            );
        });
        match self.center_mode {
            CenterMode::View => {
                ui.label(
                    RichText::new(format!("X:{:.0} Z:{:.0}", view_center.x, view_center.z)).weak(),
                );
            }
            CenterMode::Custom => {
                ui.horizontal(|ui| {
                    ui.label("X");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.custom_x)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(70.0),
                    );
                    ui.label("Z");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.custom_z)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(70.0),
                    );
                });
                if let Err(message) =
                    parse_go_to(&self.custom_x, &self.custom_z).and_then(|(x, z)| {
                        if self.custom_x.trim().is_empty() && self.custom_z.trim().is_empty() {
                            Err("enter a custom x and z".to_string())
                        } else {
                            Ok((x, z))
                        }
                    })
                {
                    ui.label(RichText::new(message).color(ui.visuals().error_fg_color));
                }
            }
        }
    }

    fn feature_filters(&mut self, ui: &mut egui::Ui, categories: &[(String, String)]) {
        ui.horizontal(|ui| {
            ui.label(labels::SEARCH_KIND);
            let current = self.kind;
            let mut selected = current;
            egui::ComboBox::from_id_salt("search-kind")
                .selected_text(current.label())
                .show_ui(ui, |ui| {
                    for kind in FeatureKindChoice::ALL {
                        ui.selectable_value(&mut selected, kind, kind.label());
                    }
                });
            self.kind = selected;
        });
        ui.horizontal(|ui| {
            ui.label(labels::SEARCH_CATEGORY);
            let current = self.category.as_str();
            let selected_text = if current.is_empty() {
                labels::SEARCH_CATEGORY_HINT
            } else {
                categories
                    .iter()
                    .find(|(id, _)| id == current)
                    .map_or(current, |(_, name)| name.as_str())
            };
            let mut selected = current.to_string();
            egui::ComboBox::from_id_salt("search-category")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut selected, String::new(), labels::SEARCH_CATEGORY_HINT);
                    for (id, name) in categories {
                        ui.selectable_value(&mut selected, id.clone(), name);
                    }
                });
            self.category = selected;
        });
        ui.horizontal(|ui| {
            ui.label(labels::SEARCH_NAME);
            ui.add(
                egui::TextEdit::singleline(&mut self.name_contains)
                    .desired_width(120.0)
                    .hint_text("any"),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.min_size_enabled, labels::SEARCH_MIN_SIZE);
            ui.add_enabled(
                self.min_size_enabled,
                egui::DragValue::new(&mut self.min_size).range(1..=1_000),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.proximity_enabled, labels::SEARCH_PROXIMITY);
            ui.add_enabled(
                self.proximity_enabled,
                egui::DragValue::new(&mut self.proximity).range(1..=1_000),
            );
            ui.label(RichText::new(labels::SEARCH_PROXIMITY_BLOCKS).weak());
        });
    }

    fn biome_filters(&mut self, ui: &mut egui::Ui, biomes: &[BiomeChoice]) {
        ui.horizontal(|ui| {
            ui.label(labels::SEARCH_BIOME);
            if biomes.is_empty() {
                ui.label(RichText::new(labels::SEARCH_BIOME_UNKNOWN).weak());
                return;
            }
            if self.biome.is_none() {
                self.biome = biomes.first().map(|(id, _, _)| *id);
            }
            let selected = self
                .biome
                .and_then(|id| biomes.iter().find(|(candidate, _, _)| *candidate == id))
                .map_or(labels::SEARCH_BIOME_UNKNOWN, |(id, name, _)| {
                    let _ = id;
                    name.as_str()
                });
            let mut chosen = self.biome;
            egui::ComboBox::from_id_salt("search-biome")
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for (id, name, _) in biomes {
                        ui.selectable_value(&mut chosen, Some(*id), name);
                    }
                });
            self.biome = chosen;
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.min_patch_enabled, labels::SEARCH_MIN_PATCH);
            ui.add_enabled(
                self.min_patch_enabled,
                egui::DragValue::new(&mut self.min_patch).range(1..=10_000_000),
            );
        });
        ui.horizontal(|ui| {
            ui.label(labels::SEARCH_COARSE_STEP);
            let mut selected = self.coarse_step;
            egui::ComboBox::from_id_salt("search-coarse")
                .selected_text(format!("{selected}"))
                .show_ui(ui, |ui| {
                    for step in COARSE_STEPS {
                        ui.selectable_value(&mut selected, step, format!("{step}"));
                    }
                });
            self.coarse_step = selected;
        });
    }

    fn limits_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(labels::SEARCH_RADIUS);
            let mut selected = self.radius_index;
            egui::ComboBox::from_id_salt("search-radius")
                .selected_text(format_distance(RADIUS_CHOICES[self.radius_index]))
                .show_ui(ui, |ui| {
                    for (index, radius) in RADIUS_CHOICES.iter().enumerate() {
                        ui.selectable_value(&mut selected, index, format_distance(*radius));
                    }
                });
            self.radius_index = selected.min(RADIUS_CHOICES.len() - 1);
        });
        ui.horizontal(|ui| {
            ui.label(labels::SEARCH_MAX_RESULTS);
            ui.add(egui::DragValue::new(&mut self.max_results).range(1..=1_000));
        });
    }

    fn status_and_progress(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
        }
        // Copy the displayed values out before any mutable action (the retry
        // button starts a new session through the same state).
        let search = state.search();
        let status = search.status();
        let cancel_requested = search.cancel_requested();
        let progress = search.progress();
        let results = search.results().len();
        let outcome = search.outcome();
        let error = search.error().map(str::to_string);
        let retry_request = search.last_request().cloned();

        match status {
            SearchStatus::Idle => {
                ui.label(RichText::new(labels::SEARCH_IDLE).weak());
            }
            SearchStatus::Running => {
                let text = if cancel_requested {
                    labels::SEARCH_CANCELLING.to_string()
                } else if let Some(progress) = progress {
                    format!(
                        "{} {} · {} {} · {} {}",
                        labels::SEARCH_STARTING,
                        progress.ring,
                        results,
                        labels::SEARCH_RESULTS,
                        progress.work,
                        labels::SEARCH_SCANNED,
                    )
                } else {
                    labels::SEARCH_STARTING.to_string()
                };
                ui.label(text);
            }
            SearchStatus::Done => {
                let mut text = format!(
                    "{}: {} {}",
                    labels::SEARCH_DONE,
                    results,
                    labels::SEARCH_RESULTS,
                );
                if let Some(outcome) = outcome {
                    text.push_str(&format!(" · {:.1} s", outcome.elapsed.as_secs_f64()));
                    if outcome.cancelled {
                        text.push_str(" · ");
                        text.push_str(labels::SEARCH_CANCELLED);
                    }
                    if outcome.truncated {
                        text.push_str(" · ");
                        text.push_str(labels::SEARCH_TRUNCATED);
                    }
                }
                ui.label(text);
                if results == 0 {
                    ui.label(RichText::new(labels::SEARCH_NO_RESULTS).weak());
                }
            }
            SearchStatus::Failed => {
                if let Some(error) = error {
                    ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
                }
                if ui.button(labels::SEARCH_RETRY).clicked()
                    && let Some(request) = retry_request
                {
                    self.start_request(state, request);
                }
            }
        }
    }

    fn results_table(&mut self, ui: &mut egui::Ui, state: &mut AppState, biomes: &[BiomeChoice]) {
        let len = state.search().results().len();
        if len == 0 {
            return;
        }
        ui.separator();
        ui.label(format!("{len} {}", labels::SEARCH_RESULTS));
        egui::ScrollArea::vertical()
            .id_salt("search-results")
            .max_height(240.0)
            .auto_shrink([false, true])
            .show_rows(ui, ROW_HEIGHT, len, |ui, range| {
                for index in range {
                    let Some(hit) = state.search().results().get(index).cloned() else {
                        continue;
                    };
                    self.result_row(ui, state, index, &hit, biomes);
                }
            });
    }

    fn result_row(
        &mut self,
        ui: &mut egui::Ui,
        state: &mut AppState,
        index: usize,
        hit: &SearchHit,
        biomes: &[BiomeChoice],
    ) {
        let name = display_name(hit, biomes);
        let mut label = format!(
            "{}  ·  {}  ·  {}  ·  {}",
            name,
            hit.category,
            format_coordinates(hit),
            format_distance(hit.distance),
        );
        if let Some(area) = format_patch_area(hit) {
            label.push_str("  ·  ");
            label.push_str(&area);
        }
        let selected = state.search().is_selected(&hit.key);
        let color = hit_color(hit, biomes);
        let response = ui
            .horizontal(|ui| {
                ui.label(RichText::new("●").color(color));
                ui.add_sized(
                    [ui.available_width(), ROW_HEIGHT],
                    egui::SelectableLabel::new(selected, label),
                )
            })
            .inner;
        response
            .clone()
            .on_hover_text(format!("{} · {}", hit.name, hit.teleport));

        if response.double_clicked() {
            state
                .view_mut()
                .set_center(f64::from(hit.x), f64::from(hit.z));
            state.view_mut().zoom_at_center(-1.0);
            state.search_mut().select(index, Instant::now());
        } else if response.clicked() {
            state
                .view_mut()
                .set_center(f64::from(hit.x), f64::from(hit.z));
            state.search_mut().select(index, Instant::now());
            state.set_status(format!("{} {}", labels::STATUS_SEARCH_SELECTED, hit.name));
        }
        if response.secondary_clicked() {
            ui.ctx().copy_text(hit.teleport.clone());
            state.set_status(format!("{} {}", labels::STATUS_SEARCH_COPIED, hit.teleport));
        }
    }

    /// The form as a request. Pure enough to unit-test without a window; the
    /// panel's own validation error is returned as a string.
    pub fn build_request(&self, state: &AppState) -> Result<SearchRequest, String> {
        let center = match self.center_mode {
            CenterMode::View => {
                let center = state.view().center();
                (center.x, center.z)
            }
            CenterMode::Custom => {
                let (x, z) = parse_go_to(&self.custom_x, &self.custom_z)?;
                (f64::from(x), f64::from(z))
            }
        };
        let target = match self.mode {
            SearchMode::Features => {
                let categories: Vec<String> = self
                    .category
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect();
                let name_contains = self.name_contains.trim();
                SearchTarget::Features(FeatureFilter {
                    kinds: self.kind.mask(),
                    categories: (!categories.is_empty()).then_some(categories),
                    name_contains: (!name_contains.is_empty()).then(|| name_contains.to_string()),
                    min_size: self.min_size_enabled.then_some(self.min_size.max(1)),
                })
            }
            SearchMode::Biome => SearchTarget::Biome(
                self.biome
                    .ok_or_else(|| "choose a biome (the loaded world defines none)".to_string())?,
            ),
        };
        let proximity = self.proximity_enabled.then(|| ProximityFilter {
            min_separation: f64::from(self.proximity.max(1)),
            neighbor_kinds: FeatureKindMask::ALL,
        });
        Ok(SearchRequest {
            target,
            center,
            max_radius: RADIUS_CHOICES[self.radius_index.min(RADIUS_CHOICES.len() - 1)],
            max_results: self.max_results.max(1),
            proximity,
            coarse_step: self.coarse_step,
            min_patch_blocks: self.min_patch_enabled.then_some(self.min_patch.max(1)),
            region_size: DEFAULT_REGION_SIZE,
        })
    }

    /// Starts `request` against the current source. Kept separate from
    /// [`Self::build_request`] so the retry button reuses the last request.
    fn start_request(&mut self, state: &mut AppState, request: SearchRequest) {
        let Some(source) = state.source_arc() else {
            self.error = Some(labels::SEARCH_NEED_SOURCE.to_string());
            return;
        };
        let started = match request.target.clone() {
            SearchTarget::Features(_) => {
                let source: Arc<dyn FeatureSource> = source;
                state
                    .search_mut()
                    .start(request, SearchSource::Features(source))
            }
            SearchTarget::Biome(_) => {
                let source: Arc<dyn WorldSource> = source;
                state
                    .search_mut()
                    .start(request, SearchSource::Biomes(source))
            }
        };
        if started {
            self.error = None;
            state.set_status(labels::STATUS_SEARCH_STARTED);
        } else {
            self.error = state.search().error().map(str::to_string);
        }
    }

    fn biome_choices(&self, state: &AppState) -> Vec<BiomeChoice> {
        let Some(source) = state.source() else {
            return Vec::new();
        };
        let mut choices: Vec<BiomeChoice> = source
            .biome_catalog()
            .iter()
            .map(|info| (info.id, info.name.clone(), info.map_color_rgb()))
            .collect();
        choices.sort_by(|left, right| left.1.cmp(&right.1));
        choices
    }

    /// The categories present in the overlay's catalogs (quick + full),
    /// `(id, display name)`, sorted and deduped.
    fn category_choices(&self, state: &AppState) -> Vec<(String, String)> {
        let mut choices = Vec::new();
        for catalog in [state.features().quick_catalog(), state.features().catalog()]
            .into_iter()
            .flatten()
        {
            for category in catalog.categories() {
                if !choices
                    .iter()
                    .any(|(id, _): &(String, String)| id == &category.id)
                {
                    choices.push((category.id.clone(), category.display_name.clone()));
                }
            }
        }
        choices.sort_by(|left, right| left.1.cmp(&right.1));
        choices
    }
}

fn display_name(hit: &SearchHit, biomes: &[BiomeChoice]) -> String {
    if let HitKind::Biome(id) = hit.kind
        && let Some((_, name, _)) = biomes.iter().find(|(biome, _, _)| *biome == id)
    {
        return name.clone();
    }
    hit.name.clone()
}

fn hit_color(hit: &SearchHit, biomes: &[BiomeChoice]) -> Color32 {
    match hit.kind {
        HitKind::Biome(id) => biomes
            .iter()
            .find(|(biome, _, _)| *biome == id)
            .map_or(Color32::from_rgb(120, 220, 160), |(_, _, [r, g, b])| {
                Color32::from_rgb(*r, *g, *b)
            }),
        HitKind::Feature(kind) => {
            let feature = Feature {
                kind,
                name: hit.name.clone(),
                category: hit.category.clone(),
                position: (hit.x, hit.y.unwrap_or(0), hit.z),
                bounds: None,
                metadata: FeatureMetadata {
                    variant: None,
                    size: None,
                    source: None,
                },
            };
            let [r, g, b, _] = category_for(&feature).color;
            Color32::from_rgb(r, g, b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    fn panel() -> SearchPanel {
        SearchPanel::default()
    }

    #[test]
    fn search_panel_builds_a_feature_request_with_every_filter() {
        let state = AppState::new("/nonexistent/world");
        let mut panel = panel();
        panel.kind = FeatureKindChoice::CaveEntrance;
        panel.category = "prefab/unique/Unique".to_string();
        panel.name_contains = "  Keep ".to_string();
        panel.min_size_enabled = true;
        panel.min_size = 7;
        panel.proximity_enabled = true;
        panel.proximity = 48;

        let request = panel.build_request(&state).expect("a feature request");
        assert_eq!(request.max_radius, RADIUS_CHOICES[2]);
        assert_eq!(request.center, (0.0, 0.0), "the view center");
        let SearchTarget::Features(filter) = request.target else {
            panic!("feature mode must build a feature target");
        };
        assert!(filter.kinds.contains(FeatureKind::CaveEntrance));
        assert_eq!(
            filter.categories,
            Some(vec!["prefab/unique/Unique".to_string()])
        );
        assert_eq!(filter.name_contains.as_deref(), Some("Keep"));
        assert_eq!(filter.min_size, Some(7));
        assert_eq!(request.proximity.expect("proximity").min_separation, 48.0);
    }

    #[test]
    fn search_panel_builds_a_biome_request_and_requires_a_biome() {
        let state = AppState::new("/nonexistent/world");
        let mut panel = panel();
        panel.mode = SearchMode::Biome;
        assert!(panel.build_request(&state).is_err(), "no biome selected");

        panel.biome = Some(BiomeId(3));
        panel.min_patch_enabled = true;
        panel.min_patch = 4_096;
        panel.coarse_step = 8;
        let request = panel.build_request(&state).expect("a biome request");
        assert_eq!(request.target, SearchTarget::Biome(BiomeId(3)));
        assert_eq!(request.min_patch_blocks, Some(4_096));
        assert_eq!(request.coarse_step, 8);
    }

    #[test]
    fn search_panel_custom_center_parses_and_rejects_bad_text() {
        let state = AppState::new("/nonexistent/world");
        let mut panel = panel();
        panel.center_mode = CenterMode::Custom;
        panel.custom_x = "-1203".to_string();
        panel.custom_z = "455".to_string();
        let request = panel.build_request(&state).expect("a custom center");
        assert_eq!(request.center, (-1203.0, 455.0));

        panel.custom_z = "north".to_string();
        assert!(panel.build_request(&state).is_err());
    }

    #[test]
    fn search_panel_category_hint_and_kind_labels_agree() {
        // Every kind choice has a non-empty label and maps to a non-empty
        // mask; "all kinds" covers the four general kinds and deliberately
        // leaves cave entrances to their own choice (the T27 density).
        for choice in FeatureKindChoice::ALL {
            assert!(!choice.label().is_empty(), "{choice:?}");
            assert_ne!(choice.mask().0, 0, "{choice:?}");
        }
        let all = FeatureKindChoice::All.mask();
        assert!(!all.contains(FeatureKind::CaveEntrance));
        for kind in [
            FeatureKind::UniquePrefab,
            FeatureKind::BiomePrefab,
            FeatureKind::Spawn,
            FeatureKind::Poi,
        ] {
            assert!(all.contains(kind), "{kind:?}");
        }
    }

    #[test]
    fn search_panel_uses_the_planets_row_icon_shapes() {
        // The row icon follows the same category mapping as the overlay: a
        // cave entrance gets the cave color, not a biome-prefab one.
        let hit = SearchHit {
            key: crate::search_state::SearchHitKey::Feature {
                kind: FeatureKind::CaveEntrance,
                name: "Cave".to_string(),
                position: (0, 60, 0),
            },
            kind: HitKind::Feature(FeatureKind::CaveEntrance),
            name: "Cave".to_string(),
            category: "Caves".to_string(),
            x: 0,
            y: Some(60),
            z: 0,
            distance: 1.0,
            teleport: "/tp 0 60 0".to_string(),
            patch_area: None,
            area_capped: false,
        };
        assert_eq!(
            hit_color(&hit, &[]),
            Color32::from_rgb(176, 132, 255),
            "the cave category color",
        );
    }
}
