/// Convert B-V color index to (R, G, B) in [0..1].
pub fn bv_to_rgb(bv_raw: f32) -> (f32, f32, f32) {
    let bv = bv_raw.clamp(-0.4, 2.0);

    let r = if bv < 0.0 {
        0.61 + 0.11 * bv + 0.1 * bv * bv
    } else if bv < 0.4 {
        0.83 + 0.17 * bv
    } else {
        1.0
    };

    let g = if bv < 0.0 {
        0.70 + 0.07 * bv + 0.1 * bv * bv
    } else if bv < 0.4 {
        0.87 + 0.11 * bv
    } else if bv < 1.6 {
        1.0 - 0.28 * (bv - 0.4)
    } else {
        0.664
    };

    let b = if bv < -0.1 {
        1.0
    } else if bv < 0.5 {
        1.0 - 1.68 * (bv + 0.1)
    } else {
        0.0
    };

    (r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0))
}

pub fn event_target_value(ev: &leptos::ev::Event) -> String {
    use wasm_bindgen::JsCast;
    ev.target()
        .unwrap()
        .unchecked_into::<web_sys::HtmlInputElement>()
        .value()
}

pub fn event_target_checked(ev: &leptos::ev::Event) -> bool {
    use wasm_bindgen::JsCast;
    ev.target()
        .unwrap()
        .unchecked_into::<web_sys::HtmlInputElement>()
        .checked()
}
