use std::cmp::min;
use std::mem;

use crossfont::Metrics;
use glutin::event::{ElementState, ModifiersState};


use alacritty_terminal::index::{Column, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Rgb;
use alacritty_terminal::term::{RenderableCell, RenderableCellContent, SizeInfo};

use crate::config::Config;
use crate::event::Mouse;
use crate::renderer::rects::{RenderLine, RenderRect};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    lines: Vec<RenderLine>,
    end_offset: u16,
    num_cols: Column,
}

impl Word {
    /// Find URL at location.
    pub fn find_at(&self, point: Point) -> Option<Url> {
        for url in &self.urls {
            if (url.start()..=url.end()).contains(&point) {
                return Some(url.clone());
            }
        }
        None
    }

        
    pub fn rects(&self, metrics: &Metrics, size: &SizeInfo) -> Vec<RenderRect> {
        let end = self.end();
        self.lines
            .iter()
            .filter(|line| line.start <= end)
            .map(|line| {
                let mut rect_line = *line;
                rect_line.end = min(line.end, end);
                rect_line.rects(Flags::UNDERLINE, metrics, size)
            })
            .flatten()
            .collect()
    }

    pub fn start(&self) -> Point {
        self.lines[0].start
    }

    pub fn end(&self) -> Point {
        self.lines[self.lines.len() - 1].end.sub(self.num_cols, self.end_offset as usize)
    }
}
