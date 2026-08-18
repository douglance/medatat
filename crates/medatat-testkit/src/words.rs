//! Fixed word lists. Every string this crate emits comes from here.
//!
//! They are invented, clinical-*sounding* tokens, not sampled from any record. Keeping the
//! vocabulary in one file makes it obvious at review time that no real data can leak in.

pub const GIVEN_NAMES: &[&str] = &[
    "Alder", "Brynn", "Corvin", "Dara", "Ellis", "Fenn", "Gale", "Haven", "Ilse", "Juno",
    "Kestrel", "Linden", "Marlow", "Nova", "Orin", "Piper", "Quill", "Rowan", "Sable", "Tamsin",
];

pub const SURNAMES: &[&str] = &[
    "Ashford",
    "Bellweather",
    "Carrow",
    "Dunmore",
    "Everly",
    "Fairbank",
    "Glover",
    "Hartley",
    "Ingram",
    "Jessup",
    "Kilbride",
    "Larkin",
    "Mercer",
    "Northcott",
    "Ossory",
    "Pemberton",
];

/// Neutral clinical-note vocabulary for text and textarea values.
pub const NOTE_WORDS: &[&str] = &[
    "afebrile",
    "ambulatory",
    "baseline",
    "bilateral",
    "chart",
    "cohort",
    "documented",
    "distal",
    "episode",
    "follow-up",
    "interval",
    "lesion",
    "marked",
    "measured",
    "midline",
    "noted",
    "obtained",
    "onset",
    "postoperative",
    "proximal",
    "recorded",
    "resolved",
    "reviewed",
    "stable",
    "unremarkable",
    "visit",
];

pub const SECTION_TITLES: &[&str] = &[
    "Demographics",
    "Presentation",
    "Vitals",
    "History",
    "Imaging",
    "Pathology",
    "Operative",
    "Adjuvant Therapy",
    "Follow-up",
    "Complications",
    "Laboratory",
    "Disposition",
];

pub const FIELD_LABELS: &[&str] = &[
    "Admission",
    "Age at Diagnosis",
    "Anaesthesia",
    "Approach",
    "Biopsy",
    "Blood Loss",
    "Comorbidity",
    "Consult",
    "Discharge",
    "Dose",
    "Duration",
    "Grade",
    "Height",
    "Laterality",
    "Margin",
    "Modality",
    "Nodes Examined",
    "Onset",
    "Procedure",
    "Response",
    "Stage",
    "Temperature",
    "Weight",
];

/// Options for the R9 radio kind. Small, closed, ordinal-free sets.
pub const RADIO_SETS: &[&[(&str, &str)]] = &[
    &[("Y", "Yes"), ("N", "No"), ("U", "Unknown")],
    &[("L", "Left"), ("R", "Right"), ("B", "Bilateral")],
    &[("M", "Male"), ("F", "Female"), ("X", "Other")],
    &[("1", "Improved"), ("2", "Unchanged"), ("3", "Worsened")],
];

/// Options for the R10 select kind. Longer lists, which is why select exists at all.
pub const SELECT_SETS: &[&[(&str, &str)]] = &[
    &[
        ("CT", "Computed Tomography"),
        ("MR", "Magnetic Resonance"),
        ("US", "Ultrasound"),
        ("XR", "Radiograph"),
        ("PET", "Positron Emission Tomography"),
        ("NON", "Not Imaged"),
    ],
    &[
        ("I", "Stage I"),
        ("IIA", "Stage IIA"),
        ("IIB", "Stage IIB"),
        ("III", "Stage III"),
        ("IV", "Stage IV"),
        ("UNK", "Not Staged"),
    ],
    &[
        ("OP", "Outpatient"),
        ("IP", "Inpatient"),
        ("ED", "Emergency"),
        ("OBS", "Observation"),
        ("TR", "Transfer"),
    ],
];

/// Units, used to make numeric labels read like a real form.
pub const UNITS: &[&str] = &["mg", "mL", "cm", "kg", "mmHg", "°C", "units"];
