use kp_model::{AudienceSpec, EpochMs, LibraryMode};
use kp_store::{InitOptions, Store};

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn library() -> Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "test".parse()?,
            name: "test".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    Ok((dir, store))
}

pub fn options() -> Result<kp_derive::FunnelOptions> {
    Ok(kp_derive::FunnelOptions {
        actor: "alice".parse()?,
        audience: AudienceSpec::Explicit(["alice".parse()?].into()),
        at: EpochMs(10),
    })
}
