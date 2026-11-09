use onyx_soul::system::{StageBlueprint, SystemSpec};

pub fn all_stages() -> Vec<StageBlueprint> {
    vec![
        StageBlueprint {
            name: "boot".into(),
            systems: vec![SystemSpec {
                kind: "boot".into(),
                params: vec![],
                persistent: true,
            }],
            entities: vec![],
        },
        // StageBlueprint { name: "interview".into(), ... },
    ]
}
