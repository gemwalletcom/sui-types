// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    extensions, format_module_id,
    test_reporter::{
        FailureReason, MoveError, TestFailure, TestResults, TestRunInfo, TestStatistics,
    },
};
use anyhow::Result;
use colored::*;

use move_binary_format::{
    errors::{Location, VMResult},
    file_format::CompiledModule,
};
use move_bytecode_utils::Modules;
use move_command_line_common::error_bitset::ErrorBitset;
use move_compiler::{
    compiled_unit::NamedCompiledModule,
    unit_test::{ExpectedFailure, ModuleTestPlan, MoveErrorType, TestCase, TestPlan},
};
use move_core_types::{
    account_address::AccountAddress, effects::ChangeSet, identifier::IdentStr,
    language_storage::ModuleId, runtime_value::serialize_values, vm_status::StatusCode,
};
use move_vm_runtime::{move_vm::MoveVM, native_functions::NativeFunctionTable};
use move_vm_test_utils::{
    gas_schedule::{unit_cost_schedule, CostTable, Gas, GasStatus},
    InMemoryStorage,
};
use rayon::prelude::*;
use std::{collections::BTreeMap, io::Write, marker::Send, sync::Mutex, time::Instant};

use move_vm_runtime::native_extensions::NativeContextExtensions;

/// Test state common to all tests
pub struct SharedTestingConfig {
    report_stacktrace_on_abort: bool,
    execution_bound: u64,
    cost_table: CostTable,
    native_function_table: NativeFunctionTable,
    starting_storage_state: InMemoryStorage,
}

pub struct TestRunner {
    num_threads: usize,
    testing_config: SharedTestingConfig,
    tests: TestPlan,
}

/// Setup storage state with the set of modules that will be needed for all tests
fn setup_test_storage<'a>(
    modules: impl Iterator<Item = &'a CompiledModule>,
) -> Result<InMemoryStorage> {
    let mut storage = InMemoryStorage::new();
    let modules = Modules::new(modules);
    for module in modules
        .compute_dependency_graph()
        .compute_topological_order()?
    {
        let module_id = module.self_id();
        let mut module_bytes = Vec::new();
        module.serialize_with_version(module.version, &mut module_bytes)?;
        storage.publish_or_overwrite_module(module_id, module_bytes);
    }

    Ok(storage)
}

fn convert_clever_move_abort_error(
    abort_code: u64,
    location: &Location,
    test_info: &BTreeMap<ModuleId, NamedCompiledModule>,
) -> Option<MoveErrorType> {
    let Some(bitset) = ErrorBitset::from_u64(abort_code) else {
        return Some(MoveErrorType::Code(abort_code));
    };

    // Otherwise it should be a tagged error
    match location {
        Location::Undefined => None,
        Location::Module(module_id) => {
            let module = test_info.get(module_id)?;
            let name_constant_index = bitset.identifier_index()?;
            let name_string = std::str::from_utf8(
                &bcs::from_bytes::<Vec<u8>>(
                    &module.module.constant_pool[name_constant_index as usize].data,
                )
                .expect("Invalid UTF-8 constant name -- this is impossible"),
            )
            .expect("Invalid UTF-8 constant name -- this is impossible")
            .to_string();
            Some(MoveErrorType::ConstantName(name_string))
        }
    }
}

impl TestRunner {
    pub fn new(
        execution_bound: u64,
        num_threads: usize,
        report_stacktrace_on_abort: bool,
        tests: TestPlan,
        // TODO: maybe we should require the clients to always pass in a list of native functions so
        // we don't have to make assumptions about their gas parameters.
        native_function_table: Option<NativeFunctionTable>,
        cost_table: Option<CostTable>,
    ) -> Result<Self> {
        let modules = tests.module_info.values().map(|info| &info.module);
        let starting_storage_state = setup_test_storage(modules)?;
        let native_function_table = native_function_table.unwrap_or_else(|| {
            move_stdlib_natives::all_natives(
                AccountAddress::from_hex_literal("0x1").unwrap(),
                move_stdlib_natives::GasParameters::zeros(),
            )
        });
        Ok(Self {
            testing_config: SharedTestingConfig {
                report_stacktrace_on_abort,
                starting_storage_state,
                execution_bound,
                native_function_table,
                // TODO: our current implementation uses a unit cost table to prevent programs from
                // running indefinitely. This should probably be done in a different way, like halting
                // after executing a certain number of instructions or setting a timer.
                //
                // From the API standpoint, we should let the client specify the cost table.
                cost_table: cost_table.unwrap_or_else(unit_cost_schedule),
            },
            num_threads,
            tests,
        })
    }

    pub fn run<W: Write + Send>(self, writer: &Mutex<W>) -> Result<TestResults> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(self.num_threads)
            .build()
            .unwrap()
            .install(|| {
                let final_statistics = self
                    .tests
                    .module_tests
                    .par_iter()
                    .map(|(_, test_plan)| {
                        self.testing_config.exec_module_tests(
                            test_plan,
                            &self.tests.module_info,
                            writer,
                        )
                    })
                    .reduce(TestStatistics::new, |acc, stats| acc.combine(stats));

                Ok(TestResults::new(final_statistics, self.tests))
            })
    }

    pub fn filter(&mut self, test_name_slice: &str) {
        for (module_id, module_test) in self.tests.module_tests.iter_mut() {
            if module_id.name().as_str().contains(test_name_slice) {
                continue;
            } else {
                let tests = std::mem::take(&mut module_test.tests);
                module_test.tests = tests
                    .into_iter()
                    .filter(|(test_name, _)| {
                        let full_name =
                            format!("{}::{}", module_id.name().as_str(), test_name.as_str());
                        full_name.contains(test_name_slice)
                    })
                    .collect();
            }
        }
    }
}

// TODO: do not expose this to backend implementations
struct TestOutput<'a, 'b, W> {
    test_plan: &'a ModuleTestPlan,
    writer: &'b Mutex<W>,
}

impl<'a, 'b, W: Write> TestOutput<'a, 'b, W> {
    fn pass(&self, fn_name: &str) {
        writeln!(
            self.writer.lock().unwrap(),
            "[ {}    ] {}::{}",
            "PASS".bold().bright_green(),
            format_module_id(&self.test_plan.module_id),
            fn_name
        )
        .unwrap()
    }

    fn fail(&self, fn_name: &str) {
        writeln!(
            self.writer.lock().unwrap(),
            "[ {}    ] {}::{}",
            "FAIL".bold().bright_red(),
            format_module_id(&self.test_plan.module_id),
            fn_name,
        )
        .unwrap()
    }

    fn timeout(&self, fn_name: &str) {
        writeln!(
            self.writer.lock().unwrap(),
            "[ {} ] {}::{}",
            "TIMEOUT".bold().bright_yellow(),
            format_module_id(&self.test_plan.module_id),
            fn_name,
        )
        .unwrap();
    }
}

impl SharedTestingConfig {
    fn execute_via_move_vm(
        &self,
        test_plan: &ModuleTestPlan,
        function_name: &str,
        test_info: &TestCase,
    ) -> (
        VMResult<ChangeSet>,
        VMResult<NativeContextExtensions>,
        VMResult<Vec<Vec<u8>>>,
        TestRunInfo,
    ) {
        let move_vm = MoveVM::new(self.native_function_table.clone()).unwrap();
        let extensions = extensions::new_extensions();
        let mut session =
            move_vm.new_session_with_extensions(&self.starting_storage_state, extensions);
        let mut gas_meter = GasStatus::new(&self.cost_table, Gas::new(self.execution_bound));
        move_vm_profiler::gas_profiler_feature_enabled! {
            use move_vm_profiler::GasProfiler;
            use move_vm_types::gas::GasMeter;
            gas_meter.set_profiler(GasProfiler::init_default_cfg(
                function_name.to_owned(),
                self.execution_bound,
            ));
        }

        // TODO: collect VM logs if the verbose flag (i.e, `self.verbose`) is set

        let now = Instant::now();
        let serialized_return_values_result = session.execute_function_bypass_visibility(
            &test_plan.module_id,
            IdentStr::new(function_name).unwrap(),
            vec![], // no ty args, at least for now
            serialize_values(test_info.arguments.iter()),
            &mut gas_meter,
        );
        let mut return_result = serialized_return_values_result.map(|res| {
            res.return_values
                .into_iter()
                .map(|(bytes, _layout)| bytes)
                .collect()
        });
        if !self.report_stacktrace_on_abort {
            if let Err(err) = &mut return_result {
                err.remove_exec_state();
            }
        }
        let test_run_info = TestRunInfo::new(
            function_name.to_string(),
            now.elapsed(),
            // TODO(Gas): This doesn't look quite right...
            //            We're not computing the number of instructions executed even with a unit gas schedule.
            Gas::new(self.execution_bound)
                .checked_sub(gas_meter.remaining_gas())
                .unwrap()
                .into(),
        );
        match session.finish_with_extensions().0 {
            Ok((cs, _, extensions)) => (Ok(cs), Ok(extensions), return_result, test_run_info),
            Err(err) => (Err(err.clone()), Err(err), return_result, test_run_info),
        }
    }

    fn exec_module_tests_with_move_vm(
        &self,
        test_plan: &ModuleTestPlan,
        global_test_context: &BTreeMap<ModuleId, NamedCompiledModule>,
        output: &TestOutput<impl Write>,
    ) -> TestStatistics {
        let mut stats = TestStatistics::new();

        for (function_name, test_info) in &test_plan.tests {
            let (_cs_result, _ext_result, exec_result, test_run_info) =
                self.execute_via_move_vm(test_plan, function_name, test_info);

            match exec_result {
                Err(err) => {
                    let sub_status = err.sub_status().and_then(|status| {
                        convert_clever_move_abort_error(status, err.location(), global_test_context)
                    });
                    let actual_err =
                        MoveError(err.major_status(), sub_status, err.location().clone());
                    assert!(err.major_status() != StatusCode::EXECUTED);
                    match test_info.expected_failure.as_ref() {
                        Some(ExpectedFailure::Expected) => {
                            output.pass(function_name);
                            stats.test_success(test_run_info, test_plan);
                        }
                        Some(ExpectedFailure::ExpectedWithError(expected_err))
                            if expected_err == &actual_err =>
                        {
                            output.pass(function_name);
                            stats.test_success(test_run_info, test_plan);
                        }
                        Some(ExpectedFailure::ExpectedWithCodeDEPRECATED(code))
                            if actual_err.0 == StatusCode::ABORTED
                                && actual_err.1.is_some()
                                && actual_err.1.as_ref().unwrap() == code =>
                        {
                            output.pass(function_name);
                            stats.test_success(test_run_info, test_plan);
                        }
                        // incorrect cases
                        Some(ExpectedFailure::ExpectedWithError(expected_err)) => {
                            output.fail(function_name);
                            stats.test_failure(
                                TestFailure::new(
                                    FailureReason::wrong_error(expected_err.clone(), actual_err),
                                    test_run_info,
                                    Some(err),
                                ),
                                test_plan,
                            )
                        }
                        Some(ExpectedFailure::ExpectedWithCodeDEPRECATED(expected_code)) => {
                            output.fail(function_name);
                            stats.test_failure(
                                TestFailure::new(
                                    FailureReason::wrong_abort_deprecated(
                                        expected_code.clone(),
                                        actual_err,
                                    ),
                                    test_run_info,
                                    Some(err),
                                ),
                                test_plan,
                            )
                        }
                        None if err.major_status() == StatusCode::OUT_OF_GAS => {
                            // Ran out of ticks, report a test timeout and log a test failure
                            output.timeout(function_name);
                            stats.test_failure(
                                TestFailure::new(
                                    FailureReason::timeout(),
                                    test_run_info,
                                    Some(err),
                                ),
                                test_plan,
                            )
                        }
                        None => {
                            output.fail(function_name);
                            stats.test_failure(
                                TestFailure::new(
                                    FailureReason::unexpected_error(actual_err),
                                    test_run_info,
                                    Some(err),
                                ),
                                test_plan,
                            )
                        }
                    }
                }
                Ok(_) => {
                    // Expected the test to fail, but it executed
                    if test_info.expected_failure.is_some() {
                        output.fail(function_name);
                        stats.test_failure(
                            TestFailure::new(FailureReason::no_error(), test_run_info, None),
                            test_plan,
                        )
                    } else {
                        // Expected the test to execute fully and it did
                        output.pass(function_name);
                        stats.test_success(test_run_info, test_plan);
                    }
                }
            }
        }

        stats
    }

    // TODO: comparison of results via different backends

    fn exec_module_tests(
        &self,
        test_plan: &ModuleTestPlan,
        test_info: &BTreeMap<ModuleId, NamedCompiledModule>,
        writer: &Mutex<impl Write>,
    ) -> TestStatistics {
        let output = TestOutput { test_plan, writer };

        self.exec_module_tests_with_move_vm(test_plan, test_info, &output)
    }
}
