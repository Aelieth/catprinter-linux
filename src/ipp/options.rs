//! Standard IPP job attributes → what we render and how many copies. No vendor attributes:
//! CUPS's `ipp` backend forwards only standard ones once `media-col-supported` is advertised.

use crate::ipp::codec::{Group, Request};
use crate::ipp::media::{self, MediaHint};
use crate::render::{Layout, Preset, RenderOptions, Tone};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOptions {
    /// 3 draft, 4 normal, 5 high
    pub print_quality: i32,
    pub color_mode: String,
    pub content_optimize: String,
    pub copies: u32,
    pub media: Option<MediaHint>,
    pub job_name: String,
    pub user: String,
    pub host: Option<String>,
    pub document_name: Option<String>,
    pub document_format: Option<String>,
    pub compression: String,
    /// Attributes we understood but ignore (pdftopdf already applied them, or not applicable).
    pub ignored: Vec<String>,
}

impl Default for JobOptions {
    fn default() -> Self {
        JobOptions {
            print_quality: 4,
            color_mode: "bi-level".into(),
            content_optimize: "auto".into(),
            copies: 1,
            media: None,
            job_name: "Untitled".into(),
            user: "anonymous".into(),
            host: None,
            document_name: None,
            document_format: None,
            compression: "none".into(),
            ignored: vec![],
        }
    }
}

impl JobOptions {
    /// Extract from a Print-Job / Create-Job / Send-Document / Validate-Job request.
    pub fn from_request(req: &Request, max_copies: u32) -> JobOptions {
        let g = None; // search all groups: CUPS puts job template attrs in job-attributes, ipptool sometimes elsewhere
        let print_quality = req
            .get_int(g, "print-quality")
            .filter(|q| (3..=5).contains(q))
            .unwrap_or(4);
        let color_mode = req
            .get_str(g, "print-color-mode")
            .unwrap_or_else(|| "bi-level".into());
        let content_optimize = req
            .get_str(g, "print-content-optimize")
            .unwrap_or_else(|| "auto".into());
        let copies = req
            .get_int(g, "copies")
            .map(|c| c.clamp(1, max_copies as i32) as u32)
            .unwrap_or(1);
        let media = media_from_request(req);
        let job_name = req
            .get_str(Some(Group::OperationAttributes), "job-name")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "untitled".into());
        let user = req
            .get_str(Some(Group::OperationAttributes), "requesting-user-name")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "anonymous".into());
        let host = req.get_str(
            Some(Group::OperationAttributes),
            "job-originating-host-name",
        );
        let document_name = req.get_str(Some(Group::OperationAttributes), "document-name");
        let document_format = req.get_str(Some(Group::OperationAttributes), "document-format");
        let compression = req
            .get_str(Some(Group::OperationAttributes), "compression")
            .unwrap_or_else(|| "none".into());
        let mut ignored = vec![];
        for name in [
            "orientation-requested",
            "print-scaling",
            "overrides",
            "multiple-document-handling",
            "sides",
            "finishings",
            "output-bin",
            "printer-resolution",
            "ipp-attribute-fidelity",
            "job-hold-until",
            "job-priority",
            "page-ranges",
            "number-up",
        ] {
            if req.get(g, name).is_some() {
                ignored.push(name.to_string());
            }
        }
        JobOptions {
            print_quality,
            color_mode,
            content_optimize,
            copies,
            media,
            job_name,
            user,
            host,
            document_name,
            document_format,
            compression,
            ignored,
        }
    }

    /// Render options for this job. `is_image` = a JPEG/PNG passthrough (always tape layout).
    pub fn render_options(&self, base: &RenderOptions, is_image: bool) -> RenderOptions {
        let mut o = base.clone();
        // print-quality is style only. Tone is print-color-mode (orthogonal).
        let mut preset = match self.print_quality {
            3 => Preset::Text,
            5 => Preset::Picture,
            _ => Preset::Default,
        };
        if self.print_quality == 4 {
            match self.content_optimize.as_str() {
                "photo" | "graphic" | "graphics" => preset = Preset::Picture,
                "text" => preset = Preset::Text,
                _ => {}
            }
        }
        // CUPS ColorModel Gray sends monochrome even when the kid left the default.
        // 4 bpp is the slow photo path only: Picture + Grayscale.
        let tone = match (self.color_mode.as_str(), preset) {
            ("monochrome", Preset::Picture) => Tone::Grayscale,
            _ => Tone::BlackWhite, // bi-level, auto, or Gray+Default/Text
        };
        // Minidoc is paper size: whole-page Sheet, no trim. Style stays Text/Default/Picture.
        let sheet = !is_image && self.is_sheet_media();
        o.preset = preset;
        o.tone = tone;
        o.layout = if is_image {
            Layout::Tape
        } else if sheet {
            Layout::Sheet
        } else {
            Layout::Auto
        };
        o
    }

    pub fn is_sheet_media(&self) -> bool {
        self.media.as_ref().is_some_and(|m| m.is_document())
    }
}

/// media-col.media-size wins, then `media` keyword.
pub fn media_from_request(req: &Request) -> Option<MediaHint> {
    let x = req
        .get_member(None, "media-col", "media-size")
        .and_then(|ms| match ms {
            ipp::value::IppValue::Collection(c) => {
                let get = |k: &str| {
                    c.iter()
                        .find(|(n, _)| n.as_str() == k)
                        .and_then(|(_, v)| match v {
                            ipp::value::IppValue::Integer(i) => Some(*i),
                            _ => None,
                        })
                };
                Some((get("x-dimension")?, get("y-dimension")?))
            }
            _ => None,
        });
    if let Some((x_hmm, y_hmm)) = x {
        let name = req
            .get_member(None, "media-col", "media-size-name")
            .and_then(crate::ipp::codec::value_to_string);
        return Some(MediaHint { x_hmm, y_hmm, name });
    }
    let name = req.get_str(None, "media")?;
    let (x_hmm, y_hmm) = media::parse_media_name(&name)?;
    Some(MediaHint {
        x_hmm,
        y_hmm,
        name: Some(name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipp::codec::{parse, v_enum, v_int, v_kw, v_name, Coll, Group, Resp, Status};

    fn req_with(job: Vec<(&str, ipp::value::IppValue)>) -> Request {
        // Build a "response" shaped request then reparse — the codec is symmetric.
        let mut r = Resp::new(0x0200, Status::SuccessfulOk, 1);
        r.add(Group::OperationAttributes, "job-name", v_name("Homework"));
        r.add(
            Group::OperationAttributes,
            "requesting-user-name",
            v_name("kid"),
        );
        for (n, v) in job {
            r.add(Group::JobAttributes, n, v);
        }
        parse(r.into_bytes()).unwrap()
    }

    #[test]
    fn quality_maps_to_presets() {
        let base = RenderOptions::default();
        let o = JobOptions::from_request(&req_with(vec![("print-quality", v_enum(3))]), 10);
        let ro = o.render_options(&base, false);
        assert_eq!((ro.preset, ro.tone), (Preset::Text, Tone::BlackWhite));
        let o = JobOptions::from_request(&req_with(vec![("print-quality", v_enum(5))]), 10);
        let ro = o.render_options(&base, false);
        assert_eq!(
            (ro.preset, ro.tone),
            (Preset::Picture, Tone::BlackWhite),
            "Picture must not force grayscale"
        );
        let o = JobOptions::from_request(
            &req_with(vec![
                ("print-quality", v_enum(5)),
                ("print-color-mode", v_kw("monochrome")),
            ]),
            10,
        );
        assert_eq!(
            o.render_options(&base, false).tone,
            Tone::Grayscale,
            "Picture + monochrome is the 4 bpp photo path"
        );
        let o = JobOptions::from_request(
            &req_with(vec![
                ("print-quality", v_enum(4)),
                ("print-color-mode", v_kw("monochrome")),
            ]),
            10,
        );
        assert_eq!(
            o.render_options(&base, false).tone,
            Tone::BlackWhite,
            "Gray+Default must stay 1 bpp so the CUPS Gray default is not a crawl"
        );
        let o = JobOptions::from_request(
            &req_with(vec![("print-content-optimize", v_kw("photo"))]),
            10,
        );
        assert_eq!(o.render_options(&base, false).preset, Preset::Picture);
        assert_eq!(
            o.render_options(&base, false).tone,
            Tone::BlackWhite,
            "photo optimize is style only"
        );
        let o = JobOptions::from_request(
            &req_with(vec![
                ("print-quality", v_enum(3)),
                ("print-content-optimize", v_kw("photo")),
            ]),
            10,
        );
        assert_eq!(
            o.render_options(&base, false).preset,
            Preset::Text,
            "quality wins over content-optimize"
        );
        let o = JobOptions::from_request(&req_with(vec![]), 10);
        assert_eq!(o.render_options(&base, true).layout, Layout::Tape);
        assert_eq!(o.job_name, "Homework");
        assert_eq!(o.user, "kid");
    }

    #[test]
    fn sheet_media_keeps_style_and_disables_trim() {
        let base = RenderOptions::default();
        let mc = || {
            Coll::new()
                .add(
                    "media-size",
                    Coll::new()
                        .add("x-dimension", v_int(media::A4.x_hmm))
                        .add("y-dimension", v_int(media::A4.y_hmm))
                        .build(),
                )
                .add("media-size-name", v_kw("iso_a4_210x297mm"))
                .build()
        };
        let picture = JobOptions::from_request(
            &req_with(vec![("print-quality", v_enum(5)), ("media-col", mc())]),
            10,
        );
        let ro = picture.render_options(&base, false);
        assert_eq!(ro.preset, Preset::Picture, "Minidoc must not wipe Picture");
        assert_eq!(ro.layout, Layout::Sheet);
        assert_eq!(ro.tone, Tone::BlackWhite);
        assert_eq!(picture.media.as_ref().unwrap().x_hmm, 21000);
        assert!(picture.is_sheet_media());
        let text = JobOptions::from_request(
            &req_with(vec![("print-quality", v_enum(3)), ("media-col", mc())]),
            10,
        );
        let ro = text.render_options(&base, false);
        assert_eq!(ro.preset, Preset::Text);
        assert_eq!(ro.layout, Layout::Sheet);
        let default = JobOptions::from_request(&req_with(vec![("media-col", mc())]), 10);
        let ro = default.render_options(&base, false);
        assert_eq!(ro.preset, Preset::Default);
        assert_eq!(ro.layout, Layout::Sheet);
        // JPEG passthrough stays tape even if the job claimed A4.
        let ro = picture.render_options(&base, true);
        assert_eq!(ro.layout, Layout::Tape);
        assert_eq!(ro.preset, Preset::Picture);
    }

    #[test]
    fn tape_text_can_still_be_grayscale() {
        let base = RenderOptions::default();
        let o = JobOptions::from_request(
            &req_with(vec![
                ("print-quality", v_enum(3)),
                ("print-color-mode", v_kw("monochrome")),
                ("media", v_kw("custom_cat-tape_48x297mm")),
            ]),
            10,
        );
        let ro = o.render_options(&base, false);
        assert_eq!(
            (ro.preset, ro.tone),
            (Preset::Text, Tone::BlackWhite),
            "Text stays 1 bpp; 4 bpp is Picture + Grayscale only"
        );
        assert!(ro.preset.trim());
    }

    #[test]
    fn paper_sticker_is_label_only() {
        let base = RenderOptions::default();
        let paper = JobOptions::from_request(
            &req_with(vec![
                ("print-quality", v_enum(5)),
                ("media-type", v_kw("stationery")),
            ]),
            10,
        );
        let sticker = JobOptions::from_request(
            &req_with(vec![
                ("print-quality", v_enum(5)),
                ("media-type", v_kw("labels")),
            ]),
            10,
        );
        let rp = paper.render_options(&base, false);
        let rs = sticker.render_options(&base, false);
        assert_eq!(rp.preset, rs.preset);
        assert_eq!(rp.tone, rs.tone);
        assert_eq!(rp.layout, rs.layout);
        assert_eq!(rp.preset, Preset::Picture);
        assert_eq!(rp.tone, Tone::BlackWhite);
    }

    #[test]
    fn copies_and_media() {
        let o = JobOptions::from_request(&req_with(vec![("copies", v_int(50))]), 10);
        assert_eq!(o.copies, 10);
        let mc = Coll::new()
            .add(
                "media-size",
                Coll::new()
                    .add("x-dimension", v_int(media::A4.x_hmm))
                    .add("y-dimension", v_int(media::A4.y_hmm))
                    .build(),
            )
            .add("media-size-name", v_kw("iso_a4_210x297mm"))
            .build();
        let o = JobOptions::from_request(&req_with(vec![("media-col", mc)]), 10);
        assert_eq!(o.media.as_ref().unwrap().x_hmm, media::A4.x_hmm);
        assert!(o.is_sheet_media());
        let o = JobOptions::from_request(
            &req_with(vec![("media", v_kw("custom_cat-tape_48x297mm"))]),
            10,
        );
        assert!(!o.is_sheet_media());
        let o = JobOptions::from_request(&req_with(vec![("orientation-requested", v_enum(4))]), 10);
        assert_eq!(o.ignored, vec!["orientation-requested"]);
    }
}
