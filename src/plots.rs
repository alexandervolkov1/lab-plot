//! Numeric plot history. TCP observations stay in Rust, including their metadata.
use crate::tcp_client::{Measurement, Number, Reading};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

pub const MAX_POINTS: usize = 10_000;

#[derive(Debug)]
pub enum PlotCommand {
    Add { name: String, unit: String },
    Point { name: String, time: f64, value: f64 },
    Clear { name: String },
    Bind { name: String, key: String },
    Unbind { name: String },
    Remove { name: String },
}

pub struct PlotSample {
    pub xy: [f64; 2],
    /// One shared observation for every curve receiving this sample; retains
    /// exact integer values, Unix milliseconds, source and unit.
    pub measurement: Option<Arc<Measurement>>,
    pub break_before: bool,
}

pub struct PlotSeries {
    pub unit: String,
    pub key: Option<String>,
    pub points: VecDeque<PlotSample>,
    pub color_index: usize,
    pub error: Option<String>,
    next_break: bool,
}
impl PlotSeries {
    fn new(unit: String, color_index: usize) -> Self {
        Self {
            unit,
            key: None,
            points: VecDeque::new(),
            color_index,
            error: None,
            next_break: true,
        }
    }
    fn push(&mut self, xy: [f64; 2], measurement: Option<Arc<Measurement>>) {
        if self.points.len() == MAX_POINTS {
            self.points.pop_front();
        }
        // Never connect manual coordinates with Unix-time observations.
        let changed_source = self
            .points
            .back()
            .is_some_and(|p| p.measurement.is_some() != measurement.is_some());
        self.points.push_back(PlotSample {
            xy,
            measurement,
            break_before: self.next_break || changed_source,
        });
        self.next_break = false;
    }
    /// Ranges used by the renderer; no line crosses a discontinuity.
    pub fn segments(&self) -> Vec<std::ops::Range<usize>> {
        let mut starts = vec![0];
        for (index, point) in self.points.iter().enumerate().skip(1) {
            if point.break_before {
                starts.push(index);
            }
        }
        starts.push(self.points.len());
        starts
            .windows(2)
            .filter(|w| w[0] < w[1])
            .map(|w| w[0]..w[1])
            .collect()
    }
    pub fn sample_unit(&self, index: usize) -> &str {
        match self.points[index]
            .measurement
            .as_deref()
            .map(|m| &m.reading)
        {
            Some(Reading::Value { unit, .. }) => unit,
            _ => &self.unit,
        }
    }
}

#[derive(Default)]
pub struct PlotState {
    pub series: BTreeMap<String, PlotSeries>,
}
impl PlotState {
    pub fn apply(&mut self, command: PlotCommand) -> Result<(), String> {
        match command {
            PlotCommand::Add { name, unit } => {
                let index = self.series.len();
                let series = self
                    .series
                    .entry(name)
                    .or_insert_with(|| PlotSeries::new(unit.clone(), index));
                if series.unit != unit {
                    series.next_break = true;
                }
                series.unit = unit;
            }
            PlotCommand::Point { name, time, value } => {
                let index = self.series.len();
                self.series
                    .entry(name)
                    .or_insert_with(|| PlotSeries::new(String::new(), index))
                    .push([time, value], None);
            }
            PlotCommand::Clear { name } => {
                if let Some(series) = self.series.get_mut(&name) {
                    series.points.clear();
                    series.next_break = true;
                }
            }
            PlotCommand::Bind { name, key } => {
                let series = self
                    .series
                    .get_mut(&name)
                    .ok_or_else(|| format!("Unknown curve: {name}"))?;
                if series.key.as_ref() != Some(&key) {
                    series.next_break = true;
                }
                series.key = Some(key);
                series.error = None;
            }
            PlotCommand::Unbind { name } => {
                let series = self
                    .series
                    .get_mut(&name)
                    .ok_or_else(|| format!("Unknown curve: {name}"))?;
                series.key = None;
                series.next_break = true;
                series.error = None;
            }
            PlotCommand::Remove { name } => {
                self.series.remove(&name);
            }
        }
        Ok(())
    }
    pub fn break_key(&mut self, key: &str) {
        for series in self
            .series
            .values_mut()
            .filter(|s| s.key.as_deref() == Some(key))
        {
            series.next_break = true;
        }
    }
    pub fn break_bound(&mut self) {
        for series in self.series.values_mut().filter(|s| s.key.is_some()) {
            series.next_break = true;
        }
    }
    /// Returns unit mismatch messages; instrument errors are retained per curve.
    pub fn measurement(&mut self, measurement: Measurement) -> Vec<String> {
        let observation = Arc::new(measurement);
        let mut errors = Vec::new();
        for (name, series) in &mut self.series {
            if series.key.as_deref() != Some(&observation.key) {
                continue;
            }
            match &observation.reading {
                Reading::Error(error) => {
                    series.error = Some(error.clone());
                    series.next_break = true;
                }
                Reading::Value { unit, .. } if unit != &series.unit => {
                    let error = format!(
                        "{name}: unit mismatch for {}: expected {:?}, received {:?}",
                        observation.key, series.unit, unit
                    );
                    if series.error.as_ref() != Some(&error) {
                        errors.push(error.clone());
                    }
                    series.error = Some(error);
                    series.next_break = true;
                }
                Reading::Value { value, .. } => {
                    let value = match value {
                        Number::Integer(v) => *v as f64,
                        Number::Float(v) => *v,
                    };
                    series.push(
                        [observation.completed_at_unix_ms as f64 / 1000.0, value],
                        Some(observation.clone()),
                    );
                    series.error = None;
                }
            }
        }
        errors
    }
}
