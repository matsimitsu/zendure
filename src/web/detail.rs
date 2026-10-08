//! The entities `GET /detail/{entity}` can describe.

use std::str::FromStr;

/// Parsed from the URL once, at the route, so everything past it holds a
/// valid entity. There is no car variant: its SOC is not journalled, so there
/// is no history to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailEntity {
    Solar,
    Home,
    Grid,
    Battery,
}

impl DetailEntity {
    pub const fn slug(self) -> &'static str {
        match self {
            DetailEntity::Solar => "solar",
            DetailEntity::Home => "home",
            DetailEntity::Grid => "grid",
            DetailEntity::Battery => "battery",
        }
    }

    pub fn path(self) -> String {
        format!("/detail/{}", self.slug())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnknownEntity;

impl FromStr for DetailEntity {
    type Err = UnknownEntity;

    fn from_str(slug: &str) -> Result<Self, UnknownEntity> {
        [Self::Solar, Self::Home, Self::Grid, Self::Battery]
            .into_iter()
            .find(|entity| entity.slug() == slug)
            .ok_or(UnknownEntity)
    }
}
