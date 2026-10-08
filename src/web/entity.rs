//! The four things the dashboard charts and opens a detail panel for, each
//! named, coloured and addressed once.

use std::str::FromStr;

/// Parsed from the URL once, at the route, so everything past it holds a
/// valid entity. There is no car: its SOC is not journalled, so there is no
/// history to chart or open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entity {
    Solar,
    Home,
    Grid,
    Battery,
}

impl Entity {
    pub const ALL: [Self; 4] = [Self::Solar, Self::Home, Self::Grid, Self::Battery];

    /// The URL segment, the BEM colour modifier and the flows chart's
    /// `data-*` name.
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Solar => "solar",
            Self::Home => "home",
            Self::Grid => "grid",
            Self::Battery => "battery",
        }
    }

    /// The short name a chart legend uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::Solar => "Solar",
            Self::Home => "Home",
            Self::Grid => "Grid",
            Self::Battery => "Battery",
        }
    }

    /// What a card and the detail panel's header call it.
    pub fn title(self) -> &'static str {
        match self {
            Self::Solar => "Solar production",
            Self::Home => "Home usage",
            Self::Grid => "Grid",
            Self::Battery => "Home battery",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Solar => "☀",
            Self::Home => "⌂",
            Self::Grid => "⇄",
            Self::Battery => "▮",
        }
    }

    pub fn path(self) -> String {
        format!("/detail/{}", self.slug())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnknownEntity;

impl FromStr for Entity {
    type Err = UnknownEntity;

    fn from_str(slug: &str) -> Result<Self, UnknownEntity> {
        Self::ALL
            .into_iter()
            .find(|entity| entity.slug() == slug)
            .ok_or(UnknownEntity)
    }
}
