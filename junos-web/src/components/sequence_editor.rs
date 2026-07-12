//! Shared multi-row capture-sequence editor used by the Imaging tab, the
//! Mosaic Planner and the Scheduler "Add job" form.
//!
//! Owns the `SeqFrame` row model and the ESQ XML serializer that callers feed
//! into KStars via `capture_load_sequence_file` (Imaging),
//! `scheduler_save_sequence_file` (Scheduler) or `scheduler_import_mosaic`
//! (Mosaic).

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::compat::{CameraSnapshot, FilterWheelSnapshot};
use crate::i18n::{Lang, t};

const INPUT_BASE: &str = "input input--sm font-mono";

// Frame type is a fixed KStars enum (not device-reported), so a hardcoded
// fallback is safe. For filter/format/encoding we deliberately do NOT fall
// back to canned values — an empty dropdown surfaces the fact that the
// device hasn't yet reported its options, instead of silently substituting
// values that may not match what the camera/wheel actually accepts.
const FRAME_TYPE_FALLBACK: &[&str] = &["Light", "Dark", "Bias", "Flat"];

fn frame_type_options(device: Vec<String>) -> Vec<String> {
    if device.is_empty() {
        FRAME_TYPE_FALLBACK.iter().map(|s| s.to_string()).collect()
    } else {
        device
    }
}

/// One row in the sequence builder.
#[derive(Clone)]
pub struct SeqFrame {
    pub frame_type: String,
    pub filter:     String,
    pub exposure:   String,
    pub count:      String,
    pub delay:      String,
    pub bin_x:      String,
    pub bin_y:      String,
    pub gain:       String,
    pub offset:     String,
    pub iso:        String,
    pub format:     String,
    pub encoding:   String,
}

impl Default for SeqFrame {
    fn default() -> Self {
        Self {
            frame_type: "Light".into(),
            filter:     String::new(),
            exposure:   "120".into(),
            count:      "10".into(),
            delay:      "0".into(),
            bin_x:      "1".into(),
            bin_y:      "1".into(),
            gain:       "100".into(),
            offset:     String::new(),
            iso:        String::new(),
            format:     "FITS".into(),
            encoding:   "FITS".into(),
        }
    }
}

/// Generate a minimal ESQ XML from a list of sequence frames.
///
/// `fits_dir` is written into every job's `<FITSDirectory>` (KStars parses it
/// into `SJ_LocalDirectory`, then combines it with the placeholder path). When
/// empty, the element is left empty so KStars keeps its own default location.
pub fn build_esq_xml(job_name: &str, fits_dir: &str, frames: &[SeqFrame]) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<SequenceQueue version='2.1'>\n");
    xml.push_str("<GuideDeviation enabled='false'>0</GuideDeviation>\n");
    xml.push_str("<GuideStartDeviation enabled='false'>0</GuideStartDeviation>\n");
    xml.push_str("<HFRCheck enabled='false'><HFRDeviation>0.1</HFRDeviation>\
<HFRCheckAlgorithm>0</HFRCheckAlgorithm><HFRCheckThreshold>0</HFRCheckThreshold>\
<HFRCheckFrames>1</HFRCheckFrames></HFRCheck>\n");
    xml.push_str("<RefocusOnTemperatureDelta enabled='false'>1</RefocusOnTemperatureDelta>\n");
    xml.push_str("<RefocusEveryN enabled='false'>60</RefocusEveryN>\n");
    xml.push_str("<RefocusOnMeridianFlip enabled='false'/>\n");
    for f in frames {
        let format = if f.format.is_empty() { "FITS" } else { f.format.as_str() };
        let encoding = if f.encoding.is_empty() { "FITS" } else { f.encoding.as_str() };
        let bx = if f.bin_x.is_empty() { "1" } else { f.bin_x.as_str() };
        let by = if f.bin_y.is_empty() { "1" } else { f.bin_y.as_str() };
        let delay = if f.delay.is_empty() { "0" } else { f.delay.as_str() };

        xml.push_str("<Job>\n");
        xml.push_str(&format!("<Exposure>{}</Exposure>\n", f.exposure));
        xml.push_str(&format!("<Format>{}</Format>\n<Encoding>{}</Encoding>\n", format, encoding));
        xml.push_str(&format!("<Binning><X>{}</X><Y>{}</Y></Binning>\n", bx, by));
        xml.push_str("<Frame><X>0</X><Y>0</Y><W>0</W><H>0</H></Frame>\n");
        if !f.filter.is_empty() {
            xml.push_str(&format!("<Filter>{}</Filter>\n", f.filter));
        }
        xml.push_str(&format!("<Type>{}</Type>\n", f.frame_type));
        xml.push_str(&format!("<Count>{}</Count>\n", f.count));
        xml.push_str(&format!("<Delay>{}</Delay>\n", delay));
        if !job_name.is_empty() {
            xml.push_str(&format!("<TargetName>{}</TargetName>\n", job_name));
        }
        xml.push_str("<GuideDitherPerJob>-1</GuideDitherPerJob>\n");
        xml.push_str(&format!("<FITSDirectory>{}</FITSDirectory>\n", fits_dir));
        xml.push_str("<PlaceholderFormat>/%T/%F/Light/%T_%F_%e_secs_%04d</PlaceholderFormat>\n");
        xml.push_str("<PlaceholderSuffix>0</PlaceholderSuffix>\n");
        xml.push_str("<UploadMode>0</UploadMode>\n");
        if !f.iso.is_empty() {
            xml.push_str(&format!("<ISOIndex>{}</ISOIndex>\n", f.iso));
        }
        let has_gain = !f.gain.is_empty();
        let has_offset = !f.offset.is_empty();
        if has_gain || has_offset {
            xml.push_str("<Properties>\n");
            if has_gain {
                xml.push_str(&format!(
                    "<PropertyVector name='CCD_GAIN'><OneElement name='GAIN'>{}</OneElement></PropertyVector>\n",
                    f.gain));
            }
            if has_offset {
                xml.push_str(&format!(
                    "<PropertyVector name='CCD_OFFSET'><OneElement name='OFFSET'>{}</OneElement></PropertyVector>\n",
                    f.offset));
            }
            xml.push_str("</Properties>\n");
        } else {
            xml.push_str("<Properties/>\n");
        }
        xml.push_str("<Calibration><FlatSource><Type>Manual</Type></FlatSource>\
<FlatDuration><Type>ADU</Type><Value>0</Value><Tolerance>0</Tolerance></FlatDuration>\
<PreMountPark>false</PreMountPark><PreDomePark>false</PreDomePark></Calibration>\n");
        xml.push_str("</Job>\n");
    }
    xml.push_str("</SequenceQueue>\n");
    xml
}

#[component]
pub fn SequenceEditor(
    /// Caller-owned row list. The editor reads and mutates it directly so the
    /// caller can serialize it (e.g. via `build_esq_xml`) on submit.
    frames: RwSignal<Vec<SeqFrame>>,
    /// Caller-owned destination folder. Written into each job's
    /// `<FITSDirectory>` on serialize. Defaults from `CaptureDirCtx`.
    fits_dir: RwSignal<String>,
    #[prop(into)] camera:       Signal<CameraSnapshot>,
    #[prop(into)] filter_wheel: Signal<FilterWheelSnapshot>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Pre-fill the destination folder from the server captures dir once it
    // loads, but only while the user hasn't typed a path of their own.
    let capture_dir = use_context::<crate::CaptureDirCtx>();
    Effect::new(move |_| {
        if let Some(cd) = capture_dir {
            let d = cd.0.get();
            if !d.is_empty() && fits_dir.with(|v| v.is_empty()) {
                fits_dir.set(d);
            }
        }
    });

    // Per-row "advanced" panel toggle, parallel to `frames`. Owned internally —
    // neither caller persists this state.
    let row_expanded: RwSignal<Vec<bool>> = RwSignal::new(
        (0..frames.with_untracked(|f| f.len())).map(|_| false).collect()
    );

    // Re-sync `row_expanded` length whenever frames length changes (e.g. when
    // the caller resets `frames` via `.set(...)` after a "clear form" action).
    Effect::new(move |_| {
        let n = frames.with(|f| f.len());
        row_expanded.update(|r| {
            while r.len() < n { r.push(false); }
            while r.len() > n { r.pop(); }
        });
    });

    view! {
        <div class="flex flex-col gap-1">
            // Destination folder — applies to every job in this form.
            <label class="flex items-center gap-2 text-sm mb-1">
                <span class="text-text-blue whitespace-nowrap">{move || tr().seq_dest_folder}</span>
                <input type="text"
                       class=format!("{INPUT_BASE} flex-1 min-w-0")
                       placeholder="/home/user/Pictures"
                       prop:value=move || fits_dir.get()
                       on:input=move |ev| {
                           let v = ev.target().unwrap()
                               .unchecked_into::<web_sys::HtmlInputElement>().value();
                           fits_dir.set(v);
                       } />
            </label>

            // Column headers
            <div class="grid grid-cols-[88px_1fr_72px_56px_28px_28px] gap-1 text-sm text-[#666] px-[2px] pb-1">
                <span>{move || tr().field_frame_type}</span>
                <span>{move || tr().mosaic_filter_col}</span>
                <span>{move || tr().mosaic_exp_col}</span>
                <span>{move || tr().mosaic_count_col}</span>
                <span></span>
                <span></span>
            </div>

            // Rows — re-rendered when frames list changes (add/remove).
            {move || {
                frames.get().into_iter().enumerate().map(|(idx, frame)| {
                    let ft = frame.frame_type.clone();
                    let fi = frame.filter.clone();
                    let ex = frame.exposure.clone();
                    let co = frame.count.clone();
                    let de = frame.delay.clone();
                    let bx = frame.bin_x.clone();
                    let by = frame.bin_y.clone();
                    let ga = frame.gain.clone();
                    let of = frame.offset.clone();
                    let is = frame.iso.clone();
                    let fmt_v = frame.format.clone();
                    let enc_v = frame.encoding.clone();

                    // Inline dropdown helper: <select> with device-supplied
                    // options, falling back to a free-text input when none
                    // are known yet (camera not streaming its switch-property).
                    let combo = move |
                        cur: String,
                        options: Vec<String>,
                        placeholder: &'static str,
                        extra_class: &'static str,
                        apply: std::rc::Rc<dyn Fn(&mut SeqFrame, String)>,
                    | -> leptos::prelude::AnyView {
                        if options.is_empty() {
                            let apply = apply.clone();
                            view! {
                                <input type="text"
                                       class=format!("{INPUT_BASE} {extra_class}")
                                       placeholder=placeholder
                                       prop:value=cur
                                       on:input=move |ev| {
                                           let v = ev.target().unwrap()
                                               .unchecked_into::<web_sys::HtmlInputElement>().value();
                                           let apply = apply.clone();
                                           frames.update(|fs| {
                                               if let Some(f) = fs.get_mut(idx) { apply(f, v); }
                                           });
                                       } />
                            }.into_any()
                        } else {
                            let cur_for_check = cur.clone();
                            let unknown = !cur_for_check.is_empty()
                                && !options.iter().any(|n| n == &cur_for_check);
                            let apply = apply.clone();
                            view! {
                                <select
                                    class=format!("{INPUT_BASE} {extra_class}")
                                    prop:value=cur.clone()
                                    on:change=move |ev| {
                                        let v = ev.target().unwrap()
                                            .unchecked_into::<web_sys::HtmlSelectElement>().value();
                                        let apply = apply.clone();
                                        frames.update(|fs| {
                                            if let Some(f) = fs.get_mut(idx) { apply(f, v); }
                                        });
                                    }
                                >
                                    {if unknown {
                                        let v = cur.clone();
                                        let lbl = cur.clone();
                                        view! {
                                            <option value=v disabled=true selected=true>{lbl}</option>
                                        }.into_any()
                                    } else { ().into_any() }}
                                    {options.iter().map(|n| {
                                        let v = n.clone();
                                        let lbl = n.clone();
                                        let sel = *n == cur;
                                        view! {
                                            <option value=v selected=sel>{lbl}</option>
                                        }.into_any()
                                    }).collect::<Vec<_>>()}
                                </select>
                            }.into_any()
                        }
                    };

                    view! {
                        <div class="flex flex-col gap-[2px] mb-[2px]">
                          // Top compact row
                          <div class="grid grid-cols-[88px_1fr_72px_56px_28px_28px] gap-1">
                              // Frame type dropdown
                              {
                                  let combo = combo.clone();
                                  let cur = ft.clone();
                                  move || {
                                      let opts = frame_type_options(camera.get().frame_type_options);
                                      combo(
                                          cur.clone(), opts, "Type", "w-full",
                                          std::rc::Rc::new(|f: &mut SeqFrame, v: String| f.frame_type = v),
                                      )
                                  }
                              }

                              // Filter dropdown
                              {
                                  let combo = combo.clone();
                                  let cur = fi.clone();
                                  move || {
                                      let opts = filter_wheel.get().filter_names;
                                      combo(
                                          cur.clone(), opts, "Filter", "w-full",
                                          std::rc::Rc::new(|f: &mut SeqFrame, v: String| f.filter = v),
                                      )
                                  }
                              }

                              // Exposure
                              <input type="number" min="0" step="0.1"
                                     class=format!("{INPUT_BASE} w-full")
                                     prop:value=ex
                                     on:input=move |ev| {
                                         let v = ev.target().unwrap()
                                             .unchecked_into::<web_sys::HtmlInputElement>().value();
                                         frames.update(|fs| {
                                             if let Some(f) = fs.get_mut(idx) { f.exposure = v; }
                                         });
                                     } />

                              // Count
                              <input type="number" min="1"
                                     class=format!("{INPUT_BASE} w-full")
                                     prop:value=co
                                     on:input=move |ev| {
                                         let v = ev.target().unwrap()
                                             .unchecked_into::<web_sys::HtmlInputElement>().value();
                                         frames.update(|fs| {
                                             if let Some(f) = fs.get_mut(idx) { f.count = v; }
                                         });
                                     } />

                              // Expand toggle
                              <button
                                  class="btn-icon !w-7 !h-7 !min-w-7 !min-h-7"
                                  title=move || tr().imaging_expand_all
                                  on:click=move |_| {
                                      row_expanded.update(|rs| {
                                          while rs.len() <= idx { rs.push(false); }
                                          rs[idx] = !rs[idx];
                                      });
                                  }>
                                  {move || {
                                      let open = row_expanded.get().get(idx).copied().unwrap_or(false);
                                      if open { "\u{25BE}" } else { "\u{25B8}" }
                                  }}
                              </button>

                              // Delete
                              <button
                                  class="btn-icon btn-danger !w-7 !h-7 !min-w-7 !min-h-7"
                                  on:click=move |_| {
                                      frames.update(|fs| {
                                          if fs.len() > 1 { fs.remove(idx); }
                                      });
                                      row_expanded.update(|rs| {
                                          if rs.len() > idx { rs.remove(idx); }
                                      });
                                  }>
                                  {"\u{00d7}"}
                              </button>
                          </div>

                          // Expanded advanced panel
                          {
                              let combo_adv = combo.clone();
                              let bx_v = bx.clone();
                              let by_v = by.clone();
                              let ga_v = ga.clone();
                              let of_v = of.clone();
                              let is_v = is.clone();
                              let fmt_vv = fmt_v.clone();
                              let enc_vv = enc_v.clone();
                              let de_v = de.clone();
                              move || {
                                  let open = row_expanded.get().get(idx).copied().unwrap_or(false);
                                  if !open { return ().into_any(); }
                                  let combo = combo_adv.clone();
                                  let cam = camera.get();
                                  let fmt_opts = cam.capture_format_options.clone();
                                  let enc_opts = cam.transfer_format_options.clone();
                                  let iso_opts = cam.iso_options.clone();
                                  let bx_v = bx_v.clone();
                                  let by_v = by_v.clone();
                                  let ga_v = ga_v.clone();
                                  let of_v = of_v.clone();
                                  let is_v = is_v.clone();
                                  let fmt_vv = fmt_vv.clone();
                                  let enc_vv = enc_vv.clone();
                                  let de_v = de_v.clone();
                                  let show_iso = !iso_opts.is_empty() || !is_v.is_empty();
                                  view! {
                                      <div class="flex flex-wrap gap-2 px-2 py-2 mb-1 border border-border-base rounded-[3px] bg-[rgba(10,12,20,0.5)] text-sm">
                                          <label class="flex items-center gap-1">
                                              <span class="text-text-blue">{move || tr().field_bin_x}</span>
                                              <input type="number" min="1" max="8"
                                                     class=format!("{INPUT_BASE} w-[48px]")
                                                     prop:value=bx_v
                                                     on:input=move |ev| {
                                                         let v = ev.target().unwrap()
                                                             .unchecked_into::<web_sys::HtmlInputElement>().value();
                                                         frames.update(|fs| {
                                                             if let Some(f) = fs.get_mut(idx) { f.bin_x = v; }
                                                         });
                                                     } />
                                          </label>
                                          <label class="flex items-center gap-1">
                                              <span class="text-text-blue">{move || tr().field_bin_y}</span>
                                              <input type="number" min="1" max="8"
                                                     class=format!("{INPUT_BASE} w-[48px]")
                                                     prop:value=by_v
                                                     on:input=move |ev| {
                                                         let v = ev.target().unwrap()
                                                             .unchecked_into::<web_sys::HtmlInputElement>().value();
                                                         frames.update(|fs| {
                                                             if let Some(f) = fs.get_mut(idx) { f.bin_y = v; }
                                                         });
                                                     } />
                                          </label>

                                          <label class="flex items-center gap-1">
                                              <span class="text-text-blue">{move || tr().field_gain}</span>
                                              <input type="number" step="1"
                                                     class=format!("{INPUT_BASE} w-[64px]")
                                                     prop:value=ga_v
                                                     on:input=move |ev| {
                                                         let v = ev.target().unwrap()
                                                             .unchecked_into::<web_sys::HtmlInputElement>().value();
                                                         frames.update(|fs| {
                                                             if let Some(f) = fs.get_mut(idx) { f.gain = v; }
                                                         });
                                                     } />
                                          </label>

                                          <label class="flex items-center gap-1">
                                              <span class="text-text-blue">{move || tr().field_offset}</span>
                                              <input type="number" step="1"
                                                     class=format!("{INPUT_BASE} w-[64px]")
                                                     prop:value=of_v
                                                     on:input=move |ev| {
                                                         let v = ev.target().unwrap()
                                                             .unchecked_into::<web_sys::HtmlInputElement>().value();
                                                         frames.update(|fs| {
                                                             if let Some(f) = fs.get_mut(idx) { f.offset = v; }
                                                         });
                                                     } />
                                          </label>

                                          {if show_iso {
                                              let combo = combo.clone();
                                              let cur = is_v.clone();
                                              view! {
                                                  <label class="flex items-center gap-1">
                                                      <span class="text-text-blue">{move || tr().field_iso}</span>
                                                      {combo(
                                                          cur, iso_opts, "ISO", "w-[80px]",
                                                          std::rc::Rc::new(|f: &mut SeqFrame, v: String| f.iso = v),
                                                      )}
                                                  </label>
                                              }.into_any()
                                          } else { ().into_any() }}

                                          <label class="flex items-center gap-1">
                                              <span class="text-text-blue">{move || tr().field_format}</span>
                                              {
                                                  let combo = combo.clone();
                                                  combo(
                                                      fmt_vv, fmt_opts, "FITS", "w-[88px]",
                                                      std::rc::Rc::new(|f: &mut SeqFrame, v: String| f.format = v),
                                                  )
                                              }
                                          </label>

                                          <label class="flex items-center gap-1">
                                              <span class="text-text-blue">{move || tr().field_encoding}</span>
                                              {
                                                  let combo = combo.clone();
                                                  combo(
                                                      enc_vv, enc_opts, "FITS", "w-[88px]",
                                                      std::rc::Rc::new(|f: &mut SeqFrame, v: String| f.encoding = v),
                                                  )
                                              }
                                          </label>

                                          <label class="flex items-center gap-1">
                                              <span class="text-text-blue">{move || tr().field_delay_s}</span>
                                              <input type="number" min="0" step="1"
                                                     class=format!("{INPUT_BASE} w-[56px]")
                                                     prop:value=de_v
                                                     on:input=move |ev| {
                                                         let v = ev.target().unwrap()
                                                             .unchecked_into::<web_sys::HtmlInputElement>().value();
                                                         frames.update(|fs| {
                                                             if let Some(f) = fs.get_mut(idx) { f.delay = v; }
                                                         });
                                                     } />
                                          </label>
                                      </div>
                                  }.into_any()
                              }
                          }
                        </div>
                    }
                }).collect::<Vec<_>>()
            }}

            // Add-row button
            <button
                class="btn btn--sm btn-primary self-start mt-1"
                on:click=move |_| {
                    frames.update(|fs| fs.push(SeqFrame::default()));
                    row_expanded.update(|rs| rs.push(false));
                }>
                {move || tr().mosaic_add_filter}
            </button>
        </div>
    }
}
