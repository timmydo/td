//! The outline face a program draws its text in at whichever scale its
//! surface takes: the style bytes read once, and a face fitted to the
//! bitmap grid's cell at the current scale, refitted when it changes. Every consumer
//! lays text out on that grid, so none changes its layout to take the
//! face. Nothing here reads the environment, a clock, a descriptor or the
//! filesystem: the consumer hands over the bytes (`pinned_face` reads the
//! pinned ones).

use std::sync::Arc;

use crate::face::Face;
use crate::raster::Scale;
use crate::sfnt::Error;
use crate::{CELL_HEIGHT, CELL_WIDTH};

#[derive(Clone, Debug)]
pub struct Typeface {
    regular: Arc<[u8]>,
    bold: Option<Arc<[u8]>>,
    /// The italic and bold italic styles, for a program that draws them.
    italic: Option<Arc<[u8]>>,
    bold_italic: Option<Arc<[u8]>>,
    /// The face fitted at the scale last asked for, or its refusal, and
    /// that scale; another scale's replaces it, so one atlas is held and a
    /// refused fit is not retried until the scale changes.
    face: (usize, Option<Face>),
}

impl Typeface {
    /// Refuses the styles `Face::fit` refuses at scale one, so a typeface
    /// that exists draws at every scale the grid's cell fits.
    pub fn new(regular: Vec<u8>, bold: Option<Vec<u8>>) -> Result<Self, Error> {
        let regular: Arc<[u8]> = regular.into();
        let bold: Option<Arc<[u8]>> = bold.map(Arc::from);
        let face = Face::fit(regular.clone(), bold.clone(), CELL_WIDTH, CELL_HEIGHT)?;
        Ok(Self {
            regular,
            bold,
            italic: None,
            bold_italic: None,
            face: (1, Some(face)),
        })
    }

    /// This typeface with italic and bold italic styles, either absent,
    /// refused as `Face::with_slant` refuses them at scale one.
    pub fn with_slant(
        mut self,
        italic: Option<Vec<u8>>,
        bold_italic: Option<Vec<u8>>,
    ) -> Result<Self, Error> {
        self.italic = italic.map(Arc::from);
        self.bold_italic = bold_italic.map(Arc::from);
        // The face fitted at scale one takes the slant, not a second fit.
        let upright = match std::mem::replace(&mut self.face, (1, None)) {
            (1, Some(face)) => face,
            _ => self.upright(1)?,
        };
        let face = upright.with_slant(self.italic.clone(), self.bold_italic.clone())?;
        self.face = (1, Some(face));
        Ok(self)
    }

    /// The regular and bold styles fitted to the grid's cell at `scale`.
    fn upright(&self, scale: usize) -> Result<Face, Error> {
        Face::fit(
            self.regular.clone(),
            self.bold.clone(),
            CELL_WIDTH.saturating_mul(scale),
            CELL_HEIGHT.saturating_mul(scale),
        )
    }

    /// Every style fitted to the grid's cell at `scale`.
    fn fit(&self, scale: usize) -> Result<Face, Error> {
        self.upright(scale)?
            .with_slant(self.italic.clone(), self.bold_italic.clone())
    }

    /// The face fitted to the grid's cell at `scale`, refitted when the
    /// scale changes; `None` only if the fit is refused there.
    pub fn face(&mut self, scale: Scale) -> Option<&mut Face> {
        let scale = scale.value();
        if self.face.0 != scale {
            self.face = (scale, self.fit(scale).ok());
        }
        self.face.1.as_mut()
    }
}
