//! Embedded desktop typography, independent of system font installation.
use std::borrow::Cow;

// Google's static Manrope files retain this legacy internal family name even
// at regular/medium/semibold/bold weights. Use their real name for font lookup;
// this is Manrope, not a request for an extra-light weight.
pub const UI_FONT_FAMILY: &str = "Manrope ExtraLight";
pub const MONO_FONT_FAMILY: &str = "Monaspace Neon";

const MANROPE: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/manrope/Manrope-Regular.ttf"),
    include_bytes!("../assets/fonts/manrope/Manrope-Medium.ttf"),
    include_bytes!("../assets/fonts/manrope/Manrope-SemiBold.ttf"),
    include_bytes!("../assets/fonts/manrope/Manrope-Bold.ttf"),
];
const NEON: [&[u8]; 6] = [
    include_bytes!("../assets/fonts/monaspace-neon/MonaspaceNeon-Regular.otf"),
    include_bytes!("../assets/fonts/monaspace-neon/MonaspaceNeon-Medium.otf"),
    include_bytes!("../assets/fonts/monaspace-neon/MonaspaceNeon-SemiBold.otf"),
    include_bytes!("../assets/fonts/monaspace-neon/MonaspaceNeon-Bold.otf"),
    include_bytes!("../assets/fonts/monaspace-neon/MonaspaceNeon-Italic.otf"),
    include_bytes!("../assets/fonts/monaspace-neon/MonaspaceNeon-BoldItalic.otf"),
];

pub const LICENSES: &str = concat!(
    "Manrope\n=======\n",
    include_str!("../assets/fonts/manrope/OFL.txt"),
    "\n\nMonaspace Neon v1.400\n====================\n",
    include_str!("../assets/fonts/monaspace-neon/OFL.txt"),
);

/// Register these before the component toolkit resolves its default fonts.
pub fn bundled_fonts() -> Vec<Cow<'static, [u8]>> {
    MANROPE.into_iter().chain(NEON).map(Cow::Borrowed).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_face(data: &[u8], family: &str, weight: u16, italic: bool) {
        let face = ttf_parser::Face::parse(data, 0).expect("valid embedded static font");
        assert!(!face.is_variable());
        assert_eq!(face.weight().to_number(), weight);
        assert_eq!(face.is_italic(), italic);
        assert!(face.names().into_iter().any(|name| {
            matches!(name.name_id, 1 | 16) && name.to_string().as_deref() == Some(family)
        }));
        for ch in "Aa0O1Il{}[]<>!=_".chars() {
            assert!(face.glyph_index(ch).is_some(), "missing {ch}");
        }
    }

    #[test]
    fn bundled_faces_match_selected_families_and_weights() {
        for (data, weight) in MANROPE.into_iter().zip([400, 500, 600, 700]) {
            check_face(data, UI_FONT_FAMILY, weight, false);
        }
        for (data, (weight, italic)) in NEON.into_iter().zip([
            (400, false),
            (500, false),
            (600, false),
            (700, false),
            (400, true),
            (700, true),
        ]) {
            check_face(data, MONO_FONT_FAMILY, weight, italic);
            assert!(ttf_parser::Face::parse(data, 0).unwrap().is_monospaced());
        }
        assert_eq!(bundled_fonts().len(), MANROPE.len() + NEON.len());
    }

    #[test]
    fn font_notices_are_available_in_the_executable() {
        assert_eq!(
            LICENSES
                .matches("SIL OPEN FONT LICENSE Version 1.1")
                .count(),
            2
        );
        assert!(LICENSES.contains("Copyright 2018 The Manrope Project Authors"));
        assert!(LICENSES.contains("Copyright (c) 2023, GitHub"));
        assert!(LICENSES.contains("Reserved Font Name \"Monaspace\""));
    }
}
