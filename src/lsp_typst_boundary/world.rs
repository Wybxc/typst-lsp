use std::path::{Path, PathBuf};

use chrono::{Datelike, FixedOffset, Local, TimeZone, Timelike, Utc};
use comemo::Prehashed;
use tokio::sync::OwnedRwLockReadGuard;
use typst::diag::{FileError, FileResult};
use typst::eval::{Datetime, Library};
use typst::font::{Font, FontBook};
use typst::util::Buffer;
use typst::World;

use crate::workspace::source_manager::SourceId;
use crate::workspace::Workspace;

use super::{typst_to_lsp, TypstPath, TypstSource, TypstSourceId};

pub struct WorkspaceWorld {
    workspace: OwnedRwLockReadGuard<Workspace>,
    main: SourceId,
    root_path: Option<PathBuf>,
}

impl WorkspaceWorld {
    pub fn new(
        workspace: OwnedRwLockReadGuard<Workspace>,
        main: SourceId,
        root_path: Option<PathBuf>,
    ) -> Self {
        Self {
            workspace,
            main,
            root_path,
        }
    }

    pub fn get_workspace(&self) -> &OwnedRwLockReadGuard<Workspace> {
        &self.workspace
    }
}

impl World for WorkspaceWorld {
    fn root(&self) -> &Path {
        match &self.root_path {
            Some(path) => path.as_ref(),
            None => Path::new(""),
        }
    }

    fn library(&self) -> &Prehashed<Library> {
        let workspace = self.get_workspace();
        &workspace.typst_stdlib
    }

    fn main(&self) -> &TypstSource {
        self.source(self.main.into())
    }

    fn resolve(&self, typst_path: &TypstPath) -> FileResult<TypstSourceId> {
        let lsp_uri = typst_to_lsp::path_to_uri(typst_path)
            .map_err(|_| FileError::NotFound(typst_path.to_owned()))?;
        self.get_workspace().sources.cache(lsp_uri).map(Into::into)
    }

    fn source(&self, typst_id: TypstSourceId) -> &TypstSource {
        let lsp_source = self
            .get_workspace()
            .sources
            .get_open_source_by_id(typst_id.into());
        lsp_source.as_ref()
    }

    fn book(&self) -> &Prehashed<FontBook> {
        self.get_workspace().fonts.book()
    }

    fn font(&self, id: usize) -> Option<Font> {
        let mut resources = self.get_workspace().resources.write();
        self.get_workspace().fonts.font(id, &mut resources)
    }

    fn file(&self, typst_path: &TypstPath) -> FileResult<Buffer> {
        let lsp_uri = typst_to_lsp::path_to_uri(typst_path)
            .map_err(|_| FileError::NotFound(typst_path.to_owned()))?;
        let mut resources = self.get_workspace().resources.write();
        let lsp_resource = resources.get_or_insert_resource(lsp_uri)?;
        Ok(lsp_resource.into())
    }

    fn today(&self, offset: Option<i64>) -> Option<Datetime> {
        let tz = match offset {
            Some(offset) => TypstTz::from_offset(offset)?,
            None => TypstTz::local(),
        };

        let datetime = Utc::now().with_timezone(&tz).naive_local();

        Datetime::from_ymd_hms(
            datetime.year(),
            datetime.month() as u8,
            datetime.day() as u8,
            datetime.hour() as u8,
            datetime.minute() as u8,
            datetime.second() as u8,
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum TypstTz {
    Local(Local),
    FixedOffset(FixedOffset),
}

impl TypstTz {
    /// Create a timezone with given UTC offset in hours, if the offset is within bounds
    pub fn from_offset(offset: i64) -> Option<Self> {
        const SECS_PER_HOUR: i32 = 60 * 60;
        FixedOffset::east_opt(offset as i32 * SECS_PER_HOUR).map(Self::FixedOffset)
    }

    pub fn local() -> Self {
        Self::Local(Local)
    }
}

impl TimeZone for TypstTz {
    type Offset = FixedOffset;

    fn from_offset(offset: &Self::Offset) -> Self {
        Self::FixedOffset(*offset)
    }

    fn offset_from_local_date(
        &self,
        local: &chrono::NaiveDate,
    ) -> chrono::LocalResult<Self::Offset> {
        match self {
            Self::Local(inner) => inner.offset_from_local_date(local),
            Self::FixedOffset(inner) => inner.offset_from_local_date(local),
        }
    }

    fn offset_from_local_datetime(
        &self,
        local: &chrono::NaiveDateTime,
    ) -> chrono::LocalResult<Self::Offset> {
        match self {
            Self::Local(inner) => inner.offset_from_local_datetime(local),
            Self::FixedOffset(inner) => inner.offset_from_local_datetime(local),
        }
    }

    fn offset_from_utc_date(&self, utc: &chrono::NaiveDate) -> Self::Offset {
        match self {
            Self::Local(inner) => inner.offset_from_utc_date(utc),
            Self::FixedOffset(inner) => inner.offset_from_utc_date(utc),
        }
    }

    fn offset_from_utc_datetime(&self, utc: &chrono::NaiveDateTime) -> Self::Offset {
        match self {
            Self::Local(inner) => inner.offset_from_utc_datetime(utc),
            Self::FixedOffset(inner) => inner.offset_from_utc_datetime(utc),
        }
    }
}
