// Copyright 2018-2019, Wayfair GmbH
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Blackhole benchmarking offramp
//!
//! Offramp used for benchmarking to generate latency histograms
//!
//! ## Configuration
//!
//! See [Config](struct.Config.html) for details.

use super::{Offramp, OfframpImpl};
use crate::codec::Codec;
use crate::errors::*;
use crate::offramp::prelude::make_postprocessors;
use crate::postprocessor::Postprocessors;
use crate::system::PipelineAddr;
use crate::url::TremorURL;
use crate::utils;
use crate::{Event, OpConfig};
use halfbrown::HashMap;
use hdrhistogram::serialization::{Deserializer, Serializer, V2Serializer};
use hdrhistogram::Histogram;
use serde_yaml;
use std::fmt::Display;
use std::io::{self, stdout, Read, Write};
use std::process;
use std::result;
use std::str;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    /// Number of seconds to collect data before the system is stopped.
    pub stop_after_secs: u64,
    /// Significant figures for the histogram
    pub significant_figures: u64,
    /// Number of seconds to warmup, events during this time are not
    /// accounted for in the latency measurements
    pub warmup_secs: u64,
}

/// A null offramp that records histograms
pub struct Blackhole {
    // config: Config,
    stop_after: u64,
    warmup: u64,
    has_stop_limit: bool,
    delivered: Histogram<u64>,
    run_secs: f64,
    bytes: usize,
    pipelines: HashMap<TremorURL, PipelineAddr>,
    postprocessors: Postprocessors,
}

impl OfframpImpl for Blackhole {
    fn from_config(config: &Option<OpConfig>) -> Result<Box<dyn Offramp>> {
        if let Some(config) = config {
            let config: Config = serde_yaml::from_value(config.clone())?;
            let now_ns = utils::nanotime();
            Ok(Box::new(Blackhole {
                // config: config.clone(),
                run_secs: config.stop_after_secs as f64,
                stop_after: now_ns + (config.stop_after_secs + config.warmup_secs) * 1_000_000_000,
                warmup: now_ns + config.warmup_secs * 1_000_000_000,
                has_stop_limit: config.stop_after_secs != 0,
                delivered: Histogram::new_with_bounds(
                    1,
                    1_000_000_000,
                    config.significant_figures as u8,
                )?,
                pipelines: HashMap::new(),
                postprocessors: vec![],
                bytes: 0,
            }))
        } else {
            Err("Blackhole offramp requires a config".into())
        }
    }
}

impl Offramp for Blackhole {
    fn add_pipeline(&mut self, id: TremorURL, addr: PipelineAddr) {
        self.pipelines.insert(id, addr);
    }
    fn remove_pipeline(&mut self, id: TremorURL) -> bool {
        self.pipelines.remove(&id);
        self.pipelines.is_empty()
    }
    fn on_event(&mut self, codec: &Box<dyn Codec>, _input: String, event: Event) {
        for event in event.into_iter() {
            let now_ns = utils::nanotime();

            if self.has_stop_limit && now_ns > self.stop_after {
                let mut buf = Vec::new();
                let mut serializer = V2Serializer::new();

                if let Err(e) = serializer.serialize(&self.delivered, &mut buf) {
                    error!("Failed to serialize histogram: {:?}", e);
                };
                quantiles(buf.as_slice(), stdout(), 5, 2).expect("Failed to serialize histogram");
                println!(
                    "\n\nThroughput: {:.1} MB/s",
                    (self.bytes as f64 / self.run_secs) / (1024.0 * 1024.0)
                );
                process::exit(0);
            };

            if now_ns > self.warmup {
                let delta_ns = now_ns - event.ingest_ns;
                if let Ok(v) = codec.encode(event.value) {
                    self.bytes += v.len();
                };
                self.delivered
                    .record(delta_ns)
                    .expect("HDR Histogram error");
            }
        }
    }
    fn default_codec(&self) -> &str {
        "null"
    }
    fn start(&mut self, _codec: &Box<dyn Codec>, postprocessors: &[String]) {
        self.postprocessors = make_postprocessors(postprocessors)
            .expect("failed to setup post processors for stdout");
    }
}

fn quantiles<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    quantile_precision: usize,
    ticks_per_half: u32,
) -> Result<()> {
    let hist: Histogram<u64> = Deserializer::new().deserialize(&mut reader)?;

    writer.write_all(
        format!(
            "{:>10} {:>quantile_precision$} {:>10} {:>14}\n\n",
            "Value",
            "Percentile",
            // "QuantileIteration",
            "TotalCount",
            "1/(1-Percentile)",
            quantile_precision = quantile_precision + 2 // + 2 from leading "0." for numbers
        )
        .as_ref(),
    )?;
    let mut sum = 0;
    for v in hist.iter_quantiles(ticks_per_half) {
        sum += v.count_since_last_iteration();
        if v.quantile_iterated_to() < 1.0 {
            writer.write_all(
                format!(
                    "{:12} {:1.*} {:10} {:14.2}\n",
                    v.value_iterated_to(),
                    quantile_precision,
                    //                        v.quantile(),
                    //                        quantile_precision,
                    v.quantile_iterated_to(),
                    sum,
                    1_f64 / (1_f64 - v.quantile_iterated_to()),
                )
                .as_ref(),
            )?;
        } else {
            writer.write_all(
                format!(
                    "{:12} {:1.*} {:10} {:>14}\n",
                    v.value_iterated_to(),
                    quantile_precision,
                    //                        v.quantile(),
                    //                        quantile_precision,
                    v.quantile_iterated_to(),
                    sum,
                    "inf"
                )
                .as_ref(),
            )?;
        }
    }

    fn write_extra_data<T1: Display, T2: Display, W: Write>(
        writer: &mut W,
        label1: &str,
        data1: T1,
        label2: &str,
        data2: T2,
    ) -> result::Result<(), io::Error> {
        writer.write_all(
            format!(
                "#[{:10} = {:12.2}, {:14} = {:12.2}]\n",
                label1, data1, label2, data2
            )
            .as_ref(),
        )
    }

    write_extra_data(
        &mut writer,
        "Mean",
        hist.mean(),
        "StdDeviation",
        hist.stdev(),
    )?;
    write_extra_data(&mut writer, "Max", hist.max(), "Total count", hist.len())?;
    write_extra_data(
        &mut writer,
        "Buckets",
        hist.buckets(),
        "SubBuckets",
        hist.distinct_values(),
    )?;

    Ok(())
}