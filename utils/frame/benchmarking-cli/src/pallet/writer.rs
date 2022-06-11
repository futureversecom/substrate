// This file is part of Substrate.

// Copyright (C) 2020-2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Outputs benchmark results to Rust files that can be ingested by the runtime.

use std::{
	collections::{HashMap, HashSet},
	fs,
	path::PathBuf,
};

use inflector::Inflector;
use serde::Serialize;

use crate::{pallet::command::ComponentRange, shared::UnderscoreHelper, PalletCmd};
use frame_benchmarking::{
	Analysis, AnalysisChoice, BenchmarkBatchSplitResults, BenchmarkResult, BenchmarkSelector,
	RegressionModel,
};
use frame_support::traits::StorageInfo;
use sp_core::hexdisplay::HexDisplay;
use sp_runtime::traits::Zero;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const TEMPLATE: &str = include_str!("./template.hbs");

// This is the final structure we will pass to the Handlebars template.
#[derive(Serialize, Default, Debug, Clone)]
struct TemplateData {
	args: Vec<String>,
	date: String,
	hostname: String,
	cpuname: String,
	version: String,
	pallet: String,
	instance: String,
	header: String,
	cmd: CmdData,
	benchmarks: Vec<BenchmarkData>,
}

// This was the final data we have about each benchmark.
#[derive(Serialize, Default, Debug, Clone)]
struct BenchmarkData {
	name: String,
	components: Vec<Component>,
	#[serde(serialize_with = "string_serialize")]
	base_weight: u128,
	#[serde(serialize_with = "string_serialize")]
	base_reads: u128,
	#[serde(serialize_with = "string_serialize")]
	base_writes: u128,
	#[serde(serialize_with = "string_serialize")]
	base_proof_size: u128,
	component_weight: Vec<ComponentSlope>,
	component_reads: Vec<ComponentSlope>,
	component_writes: Vec<ComponentSlope>,
	component_proof_size: Vec<ComponentSlope>,
	worst_case_proof_size: u32,
	component_ranges: Vec<ComponentRange>,
	comments: Vec<String>,
}

// This forwards some specific metadata from the `PalletCmd`
#[derive(Serialize, Default, Debug, Clone)]
struct CmdData {
	steps: u32,
	repeat: u32,
	lowest_range_values: Vec<u32>,
	highest_range_values: Vec<u32>,
	execution: String,
	wasm_execution: String,
	chain: String,
	db_cache: u32,
	analysis_choice: String,
}

// This encodes the component name and whether that component is used.
#[derive(Serialize, Debug, Clone, Eq, PartialEq)]
struct Component {
	name: String,
	is_used: bool,
}

// This encodes the slope of some benchmark related to a component.
#[derive(Serialize, Debug, Clone, Eq, PartialEq)]
struct ComponentSlope {
	name: String,
	#[serde(serialize_with = "string_serialize")]
	slope: u128,
	#[serde(serialize_with = "string_serialize")]
	error: u128,
}

// Small helper to create an `io::Error` from a string.
fn io_error(s: &str) -> std::io::Error {
	use std::io::{Error, ErrorKind};
	Error::new(ErrorKind::Other, s)
}

// This function takes a list of `BenchmarkBatch` and organizes them by pallet into a `HashMap`.
// So this: `[(p1, b1), (p1, b2), (p2, b1), (p1, b3), (p2, b2)]`
// Becomes:
//
// ```
// p1 -> [b1, b2, b3]
// p2 -> [b1, b2]
// ```
fn map_results(
	batches: &[BenchmarkBatchSplitResults],
	storage_info: &[StorageInfo],
	component_ranges: &HashMap<(Vec<u8>, Vec<u8>), Vec<ComponentRange>>,
	analysis_choice: &AnalysisChoice,
) -> Result<HashMap<(String, String), Vec<BenchmarkData>>, std::io::Error> {
	// Skip if batches is empty.
	if batches.is_empty() {
		return Err(io_error("empty batches"))
	}

	let mut all_benchmarks = HashMap::<_, Vec<BenchmarkData>>::new();

	for batch in batches {
		// Skip if there are no results
		if batch.time_results.is_empty() {
			continue
		}

		let pallet_string = String::from_utf8(batch.pallet.clone()).unwrap();
		let instance_string = String::from_utf8(batch.instance.clone()).unwrap();
		let benchmark_data =
			get_benchmark_data(batch, storage_info, &component_ranges, analysis_choice);
		let pallet_benchmarks = all_benchmarks.entry((pallet_string, instance_string)).or_default();
		pallet_benchmarks.push(benchmark_data);
	}
	Ok(all_benchmarks)
}

// Get an iterator of errors from a model. If the model is `None` all errors are zero.
fn extract_errors(model: &Option<RegressionModel>) -> impl Iterator<Item = u128> + '_ {
	let mut errors = model.as_ref().map(|m| m.se.regressor_values.iter());
	std::iter::from_fn(move || match &mut errors {
		Some(model) => model.next().map(|val| *val as u128),
		_ => Some(0),
	})
}

// Analyze and return the relevant results for a given benchmark.
fn get_benchmark_data(
	batch: &BenchmarkBatchSplitResults,
	storage_info: &[StorageInfo],
	// Per extrinsic component ranges.
	component_ranges: &HashMap<(Vec<u8>, Vec<u8>), Vec<ComponentRange>>,
	analysis_choice: &AnalysisChoice,
) -> BenchmarkData {
	// You can use this to put any additional comments with the benchmarking output.
	let mut comments = Vec::<String>::new();

	// Analyze benchmarks to get the linear regression.
	let analysis_function = match analysis_choice {
		AnalysisChoice::MinSquares => Analysis::min_squares_iqr,
		AnalysisChoice::MedianSlopes => Analysis::median_slopes,
		AnalysisChoice::Max => Analysis::max,
	};

	let extrinsic_time = analysis_function(&batch.time_results, BenchmarkSelector::ExtrinsicTime)
		.expect("analysis function should return an extrinsic time for valid inputs");
	let reads = analysis_function(&batch.db_results, BenchmarkSelector::Reads)
		.expect("analysis function should return the number of reads for valid inputs");
	let writes = analysis_function(&batch.db_results, BenchmarkSelector::Writes)
		.expect("analysis function should return the number of writes for valid inputs");
	let proof_size = analysis_function(&batch.db_results, BenchmarkSelector::ProofSize)
		.expect("analysis function should return proof sizes for valid inputs");

	// Analysis data may include components that are not used, this filters out anything whose value
	// is zero.
	let mut used_components = Vec::new();
	let mut used_extrinsic_time = Vec::new();
	let mut used_reads = Vec::new();
	let mut used_writes = Vec::new();
	let mut used_proof_size = Vec::new();

	extrinsic_time
		.slopes
		.into_iter()
		.zip(extrinsic_time.names.iter())
		.zip(extract_errors(&extrinsic_time.model))
		.for_each(|((slope, name), error)| {
			if !slope.is_zero() {
				if !used_components.contains(&name) {
					used_components.push(name);
				}
				used_extrinsic_time.push(ComponentSlope {
					name: name.clone(),
					slope: slope.saturating_mul(1000),
					error: error.saturating_mul(1000),
				});
			}
		});
	reads
		.slopes
		.into_iter()
		.zip(reads.names.iter())
		.zip(extract_errors(&reads.model))
		.for_each(|((slope, name), error)| {
			if !slope.is_zero() {
				if !used_components.contains(&name) {
					used_components.push(name);
				}
				used_reads.push(ComponentSlope { name: name.clone(), slope, error });
			}
		});
	writes
		.slopes
		.into_iter()
		.zip(writes.names.iter())
		.zip(extract_errors(&writes.model))
		.for_each(|((slope, name), error)| {
			if !slope.is_zero() {
				if !used_components.contains(&name) {
					used_components.push(name);
				}
				used_writes.push(ComponentSlope { name: name.clone(), slope, error });
			}
		});
	proof_size
		.slopes
		.into_iter()
		.zip(proof_size.names.iter())
		.zip(extract_errors(&proof_size.model))
		.for_each(|((slope, name), error)| {
			if !slope.is_zero() {
				if !used_components.contains(&name) {
					used_components.push(name);
				}
				used_proof_size.push(ComponentSlope { name: name.clone(), slope, error });
			}
		});

	// This puts a marker on any component which is entirely unused in the weight formula.
	let components = batch.time_results[0]
		.components
		.iter()
		.map(|(name, _)| -> Component {
			let name_string = name.to_string();
			let is_used = used_components.contains(&&name_string);
			Component { name: name_string, is_used }
		})
		.collect::<Vec<_>>();

	// We add additional comments showing which storage items were touched.
	// We find the worst case proof size, and use that as the final proof size result.
	let worst_case_proof_size: u32 =
		process_storage_results(&mut comments, &batch.db_results, storage_info);
	let component_ranges = component_ranges
		.get(&(batch.pallet.clone(), batch.benchmark.clone()))
		.map(|c| c.clone())
		.unwrap_or_default();

	BenchmarkData {
		name: String::from_utf8(batch.benchmark.clone()).unwrap(),
		components,
		base_weight: extrinsic_time.base.saturating_mul(1000),
		base_reads: reads.base,
		base_writes: writes.base,
		base_proof_size: proof_size.base,
		component_weight: used_extrinsic_time,
		component_reads: used_reads,
		component_writes: used_writes,
		component_proof_size: used_proof_size,
		worst_case_proof_size,
		component_ranges,
		comments,
	}
}

// Create weight file from benchmark data and Handlebars template.
pub(crate) fn write_results(
	batches: &[BenchmarkBatchSplitResults],
	storage_info: &[StorageInfo],
	component_ranges: &HashMap<(Vec<u8>, Vec<u8>), Vec<ComponentRange>>,
	path: &PathBuf,
	cmd: &PalletCmd,
) -> Result<(), std::io::Error> {
	// Use custom template if provided.
	let template: String = match &cmd.template {
		Some(template_file) => fs::read_to_string(template_file)?,
		None => TEMPLATE.to_string(),
	};

	// Use header if provided
	let header_text = match &cmd.header {
		Some(header_file) => {
			let text = fs::read_to_string(header_file)?;
			text
		},
		None => String::new(),
	};

	// Date string metadata
	let date = chrono::Utc::now().format("%Y-%m-%d").to_string();

	// Full CLI args passed to trigger the benchmark.
	let args = std::env::args().collect::<Vec<String>>();

	// Which analysis function should be used when outputting benchmarks
	let analysis_choice: AnalysisChoice =
		cmd.output_analysis.clone().try_into().map_err(io_error)?;

	// Capture individual args
	let cmd_data = CmdData {
		steps: cmd.steps,
		repeat: cmd.repeat,
		lowest_range_values: cmd.lowest_range_values.clone(),
		highest_range_values: cmd.highest_range_values.clone(),
		execution: format!("{:?}", cmd.execution),
		wasm_execution: cmd.wasm_method.to_string(),
		chain: format!("{:?}", cmd.shared_params.chain),
		db_cache: cmd.database_cache_size,
		analysis_choice: format!("{:?}", analysis_choice),
	};

	// New Handlebars instance with helpers.
	let mut handlebars = handlebars::Handlebars::new();
	handlebars.register_helper("underscore", Box::new(UnderscoreHelper));
	handlebars.register_helper("join", Box::new(JoinHelper));
	// Don't HTML escape any characters.
	handlebars.register_escape_fn(|s| -> String { s.to_string() });

	// Organize results by pallet into a JSON map
	let all_results = map_results(batches, storage_info, component_ranges, &analysis_choice)?;
	for ((pallet, instance), results) in all_results.iter() {
		let mut file_path = path.clone();
		// If a user only specified a directory...
		if file_path.is_dir() {
			// Check if there might be multiple instances benchmarked.
			if all_results.keys().any(|(p, i)| p == pallet && i != instance) {
				// Create new file: "path/to/pallet_name_instance_name.rs".
				file_path.push(pallet.clone() + "_" + instance.to_snake_case().as_str());
			} else {
				// Create new file: "path/to/pallet_name.rs".
				file_path.push(pallet.clone());
			}
			file_path.set_extension("rs");
		}

		let hbs_data = TemplateData {
			args: args.clone(),
			date: date.clone(),
			hostname: cmd.hostinfo_params.hostname(),
			cpuname: cmd.hostinfo_params.cpuname(),
			version: VERSION.to_string(),
			pallet: pallet.to_string(),
			instance: instance.to_string(),
			header: header_text.clone(),
			cmd: cmd_data.clone(),
			benchmarks: results.clone(),
		};

		let mut output_file = fs::File::create(file_path)?;
		handlebars
			.render_template_to_write(&template, &hbs_data, &mut output_file)
			.map_err(|e| io_error(&e.to_string()))?;
	}
	Ok(())
}

// This function looks at the keys touched during the benchmark, and the storage info we collected
// from the pallets, and creates comments with information about the storage keys touched during
// each benchmark.
//
// It returns the max PoV size used by all the storage accesses from these results.
pub(crate) fn process_storage_results(
	comments: &mut Vec<String>,
	results: &[BenchmarkResult],
	storage_info: &[StorageInfo],
) -> u32 {
	let mut storage_info_map = storage_info
		.iter()
		.map(|info| (info.prefix.clone(), info))
		.collect::<HashMap<_, _>>();

	// Special hack to show `Skipped Metadata`
	let skip_storage_info = StorageInfo {
		pallet_name: b"Skipped".to_vec(),
		storage_name: b"Metadata".to_vec(),
		prefix: b"Skipped Metadata".to_vec(),
		max_values: None,
		max_size: None,
	};
	storage_info_map.insert(skip_storage_info.prefix.clone(), &skip_storage_info);

	// Special hack to show `Benchmark Override`
	let benchmark_override = StorageInfo {
		pallet_name: b"Benchmark".to_vec(),
		storage_name: b"Override".to_vec(),
		prefix: b"Benchmark Override".to_vec(),
		max_values: None,
		max_size: None,
	};
	storage_info_map.insert(benchmark_override.prefix.clone(), &benchmark_override);

	// This tracks the keys we already identified, so we only generate a single comment.
	let mut identified_prefix = HashSet::<Vec<u8>>::new();
	let mut identified_key = HashSet::<Vec<u8>>::new();

	let mut max_pov: u32 = 0;

	for result in results {
		for (key, reads, writes, whitelisted) in &result.keys {
			// skip keys which are whitelisted
			if *whitelisted {
				continue
			}
			let prefix_length = key.len().min(32);
			let prefix = key[0..prefix_length].to_vec();
			let is_key_identified = identified_key.contains(key);
			let is_prefix_identified = identified_prefix.contains(&prefix);

			match (is_key_identified, is_prefix_identified) {
				// We already did everything, move on...
				(true, true) => continue,
				// New key, but an existing prefix, we just add the base storage size, since
				// trie impact should already be accounted for when we looked at the prefix last.
				(false, true) => {
					// track newly identified key
					identified_key.insert(key.clone());
				},
				// New key and prefix. Calculate the total worst case PoV including the trie.
				(false, false) => {
					// track newly identified key and prefix
					identified_key.insert(key.clone());
					identified_prefix.insert(prefix.clone());
				},
				// Not possible. If the key is known, the prefix is too.
				(true, false) => unreachable!(),
			}

			// For any new prefix, we should write some comment
			if !is_prefix_identified {
				match storage_info_map.get(&prefix) {
					Some(key_info) => {
						let comment = format!(
							"Storage: {} {} (r:{} w:{})",
							String::from_utf8(key_info.pallet_name.clone())
								.expect("encoded from string"),
							String::from_utf8(key_info.storage_name.clone())
								.expect("encoded from string"),
							reads,
							writes,
						);
						comments.push(comment)
					},
					None => {
						let comment = format!(
							"Storage: unknown [0x{}] (r:{} w:{})",
							HexDisplay::from(key),
							reads,
							writes,
						);
						comments.push(comment)
					},
				}
			}

			// For any new key, we should add the PoV impact.
			if !is_key_identified {
				match storage_info_map.get(&prefix) {
					Some(key_info) => {
						match worst_case_pov(
							key_info.max_values,
							key_info.max_size,
							!is_prefix_identified,
						) {
							Some(new_pov) => max_pov += new_pov,
							None => {
								let comment = format!(
									"Storage Proof Skipped: {} {}",
									String::from_utf8(key_info.pallet_name.clone())
										.expect("encoded from string"),
									String::from_utf8(key_info.storage_name.clone())
										.expect("encoded from string"),
								);
								comments.push(comment)
							},
						}
					},
					None => {
						let comment = format!(
							"Storage Proof Skipped: unknown [0x{}] (r:{} w:{})",
							HexDisplay::from(key),
							reads,
							writes,
						);
						comments.push(comment)
					},
				}
			}
		}
	}

	max_pov
}

// Given the max values and max size of some storage item, calculate the worst
// case PoV
fn worst_case_pov(
	max_values: Option<u32>,
	max_size: Option<u32>,
	is_new_prefix: bool,
) -> Option<u32> {
	if let Some(max_size) = max_size {
		let trie_size: u32 = if is_new_prefix {
			// Assume worst case map of 6 layers.
			let max_values = max_values.unwrap_or(16u32.pow(6));
			let depth: u32 = easy_log_16(max_values);
			// 16 items per depth layer, each containing a 32 byte hash.
			depth * 16 * 32
		} else {
			0
		};

		Some(trie_size + max_size)
	} else {
		None
	}
}

// A really basic loop which calculates Log 16 of some value.
fn easy_log_16(input: u32) -> u32 {
	for i in 0..7 {
		if input <= 16u32.pow(i) {
			return i + 1
		}
	}

	// u32 supports up to 16^8
	8
}

// A helper to join a string of vectors.
#[derive(Clone, Copy)]
struct JoinHelper;
impl handlebars::HelperDef for JoinHelper {
	fn call<'reg: 'rc, 'rc>(
		&self,
		h: &handlebars::Helper,
		_: &handlebars::Handlebars,
		_: &handlebars::Context,
		_rc: &mut handlebars::RenderContext,
		out: &mut dyn handlebars::Output,
	) -> handlebars::HelperResult {
		use handlebars::JsonRender;
		let param = h.param(0).unwrap();
		let value = param.value();
		let joined = if value.is_array() {
			value
				.as_array()
				.unwrap()
				.iter()
				.map(|v| v.render())
				.collect::<Vec<String>>()
				.join(" ")
		} else {
			value.render()
		};
		out.write(&joined)?;
		Ok(())
	}
}

// u128 does not serialize well into JSON for `handlebars`, so we represent it as a string.
fn string_serialize<S>(x: &u128, s: S) -> Result<S::Ok, S::Error>
where
	S: serde::Serializer,
{
	s.serialize_str(&x.to_string())
}

#[cfg(test)]
mod test {
	use super::*;
	use frame_benchmarking::{BenchmarkBatchSplitResults, BenchmarkParameter, BenchmarkResult};

	fn test_data(
		pallet: &[u8],
		benchmark: &[u8],
		param: BenchmarkParameter,
		base: u32,
		slope: u32,
	) -> BenchmarkBatchSplitResults {
		let mut results = Vec::new();
		for i in 0..5 {
			results.push(BenchmarkResult {
				components: vec![(param, i), (BenchmarkParameter::z, 0)],
				extrinsic_time: (base + slope * i).into(),
				storage_root_time: (base + slope * i).into(),
				reads: (base + slope * i).into(),
				repeat_reads: 0,
				writes: (base + slope * i).into(),
				repeat_writes: 0,
				proof_size: (i + 1) * 1024,
				keys: vec![],
			})
		}

		return BenchmarkBatchSplitResults {
			pallet: [pallet.to_vec(), b"_pallet".to_vec()].concat(),
			instance: b"instance".to_vec(),
			benchmark: [benchmark.to_vec(), b"_benchmark".to_vec()].concat(),
			time_results: results.clone(),
			db_results: results,
		}
	}

	fn check_data(benchmark: &BenchmarkData, component: &str, base: u128, slope: u128) {
		assert_eq!(
			benchmark.components,
			vec![
				Component { name: component.to_string(), is_used: true },
				Component { name: "z".to_string(), is_used: false },
			],
		);
		// Weights multiplied by 1,000
		assert_eq!(benchmark.base_weight, base * 1_000);
		assert_eq!(
			benchmark.component_weight,
			vec![ComponentSlope { name: component.to_string(), slope: slope * 1_000, error: 0 }]
		);
		// DB Reads/Writes are untouched
		assert_eq!(benchmark.base_reads, base);
		assert_eq!(
			benchmark.component_reads,
			vec![ComponentSlope { name: component.to_string(), slope, error: 0 }]
		);
		assert_eq!(benchmark.base_writes, base);
		assert_eq!(
			benchmark.component_writes,
			vec![ComponentSlope { name: component.to_string(), slope, error: 0 }]
		);
		assert_eq!(benchmark.base_proof_size, 1024);
		assert_eq!(
			benchmark.component_proof_size,
			vec![ComponentSlope { name: component.to_string(), slope: 1024, error: 0 }]
		);
	}

	#[test]
	fn map_results_works() {
		let mapped_results = map_results(
			&[
				test_data(b"first", b"first", BenchmarkParameter::a, 10, 3),
				test_data(b"first", b"second", BenchmarkParameter::b, 9, 2),
				test_data(b"second", b"first", BenchmarkParameter::c, 3, 4),
			],
			&[],
			&Default::default(),
			&AnalysisChoice::default(),
		)
		.unwrap();

		let first_benchmark = &mapped_results
			.get(&("first_pallet".to_string(), "instance".to_string()))
			.unwrap()[0];
		assert_eq!(first_benchmark.name, "first_benchmark");
		check_data(first_benchmark, "a", 10, 3);

		let second_benchmark = &mapped_results
			.get(&("first_pallet".to_string(), "instance".to_string()))
			.unwrap()[1];
		assert_eq!(second_benchmark.name, "second_benchmark");
		check_data(second_benchmark, "b", 9, 2);

		let second_pallet_benchmark = &mapped_results
			.get(&("second_pallet".to_string(), "instance".to_string()))
			.unwrap()[0];
		assert_eq!(second_pallet_benchmark.name, "first_benchmark");
		check_data(second_pallet_benchmark, "c", 3, 4);
	}

	#[test]
	fn template_works() {
		let all_results = map_results(
			&[
				test_data(b"first", b"first", BenchmarkParameter::a, 10, 3),
				test_data(b"first", b"second", BenchmarkParameter::b, 9, 2),
				test_data(b"second", b"first", BenchmarkParameter::c, 3, 4),
			],
			&[],
			&Default::default(),
			&AnalysisChoice::default(),
		)
		.unwrap();

		// New Handlebars instance with helpers.
		let mut handlebars = handlebars::Handlebars::new();
		handlebars.register_helper("underscore", Box::new(UnderscoreHelper));
		handlebars.register_helper("join", Box::new(JoinHelper));
		// Don't HTML escape any characters.
		handlebars.register_escape_fn(|s| -> String { s.to_string() });

		for ((_pallet, _instance), results) in all_results.iter() {
			let hbs_data = TemplateData { benchmarks: results.clone(), ..Default::default() };

			let output = handlebars.render_template(&TEMPLATE, &hbs_data);
			assert!(output.is_ok());
			println!("{:?}", output);
		}
	}
}
