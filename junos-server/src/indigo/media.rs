//! Preview frames: which module a fetched JPEG belongs to, and the
//! `new_preview_image` the browser expects for it.
//!
//! The browser routes a preview on the `uuid` prefix of its metadata
//! (`ws/store.rs`): `+F` focus (with the star overlay), `+G` guide, `+A`
//! align / polar align (paired with the next solution when `view` and
//! `resolution` are present), anything else the Imaging tab. KStars uses the
//! same prefixes (`media.cpp`); this backend mints them itself, one counter
//! per module.

use serde_json::json;

use super::commands::reply_msg;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Imaging,
    Focus,
    Guide,
    Align,
}

impl Source {
    fn prefix(self) -> &'static str {
        match self {
            Self::Imaging => "+P",
            Self::Focus => "+F",
            Self::Guide => "+G",
            Self::Align => "+A",
        }
    }
}

/// A preview JPEG fetched from a BLOB link, with its pixel size.
pub struct Fetched {
    pub source: Source,
    pub jpeg: Vec<u8>,
    pub size: (u32, u32),
}

#[derive(Default)]
pub struct Media {
    seq: [u64; 4],
}

impl Media {
    /// The `new_preview_image` for a fetched JPEG, in the shape
    /// `kstars_ws.rs::decode_media_frame` produces (inline base64, KStars'
    /// media metadata). No raw statistics: the browser's histogram works on
    /// the display axis only.
    pub fn preview_msg(&mut self, f: &Fetched) -> String {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        let n = &mut self.seq[f.source as usize];
        *n += 1;
        let (w, h) = f.size;
        let mut metadata = json!({
            "uuid": format!("{}{}", f.source.prefix(), *n),
            "ext": "jpg",
            "resolution": format!("{w}x{h}"),
            "bpp": "8",
        });
        if f.source == Source::Align {
            // The sky keeps an align frame as the solve's image only when the
            // metadata says which view it is (any string) and gives its size.
            metadata["view"] = json!("align");
        }
        let mut payload = json!({ "metadata": metadata, "data": BASE64.encode(&f.jpeg) });
        crate::starfind::annotate_focus_payload(&mut payload, &f.jpeg, &metadata);
        reply_msg("new_preview_image", payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn msg(media: &mut Media, source: Source) -> Value {
        let f = Fetched { source, jpeg: b"\xff\xd8".to_vec(), size: (640, 480) };
        serde_json::from_str(&media.preview_msg(&f)).unwrap()
    }

    #[test]
    fn prefixes_and_counters() {
        let mut m = Media::default();
        let v = msg(&mut m, Source::Imaging);
        assert_eq!(v["type"], "new_preview_image");
        assert_eq!(v["payload"]["metadata"]["uuid"], "+P1");
        assert_eq!(v["payload"]["metadata"]["resolution"], "640x480");
        assert_eq!(v["payload"]["metadata"]["ext"], "jpg");
        assert_eq!(v["payload"]["data"], "/9g=");
        assert!(v["payload"]["metadata"].get("view").is_none());
        assert_eq!(msg(&mut m, Source::Imaging)["payload"]["metadata"]["uuid"], "+P2");
        assert_eq!(msg(&mut m, Source::Focus)["payload"]["metadata"]["uuid"], "+F1");
        assert_eq!(msg(&mut m, Source::Guide)["payload"]["metadata"]["uuid"], "+G1");
        let a = msg(&mut m, Source::Align);
        assert_eq!(a["payload"]["metadata"]["uuid"], "+A1");
        assert_eq!(a["payload"]["metadata"]["view"], "align");
    }

    #[test]
    fn focus_frame_gets_the_star_overlay() {
        // A real JPEG with one bright star on a dark field.
        let w = 64u32;
        let h = 48u32;
        let mut img = image::GrayImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let dx = x as f64 - 30.0;
            let dy = y as f64 - 20.0;
            let v = 10.0 + 220.0 * (-(dx * dx + dy * dy) / 4.0).exp();
            *p = image::Luma([v as u8]);
        }
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let mut m = Media::default();
        let f = Fetched { source: Source::Focus, jpeg, size: (w, h) };
        let v: Value = serde_json::from_str(&m.preview_msg(&f)).unwrap();
        // JPEG ringing can split the halo into satellite blobs: the brightest
        // detection must sit on the painted star.
        let stars = v["payload"]["stars"].as_array().unwrap();
        assert!(!stars.is_empty());
        assert!((stars[0]["x"].as_f64().unwrap() - 30.0).abs() < 1.5, "{stars:?}");
        assert!((stars[0]["y"].as_f64().unwrap() - 20.0).abs() < 1.5, "{stars:?}");
        assert_eq!(v["payload"]["star_w"], w);
        assert_eq!(v["payload"]["star_scale"], 1.0);
    }
}
