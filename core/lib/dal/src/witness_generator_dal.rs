use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

use itertools::Itertools;
use sqlx::Row;

use crate::models::storage_witness_job_info::StorageWitnessJobInfo;
use zksync_object_store::gcs_utils::merkle_tree_paths_blob_url;
use zksync_object_store::gcs_utils::{
    aggregation_outputs_blob_url, basic_circuits_blob_url, basic_circuits_inputs_blob_url,
    final_node_aggregations_blob_url, leaf_layer_subqueues_blob_url, scheduler_witness_blob_url,
};
use zksync_types::proofs::{
    AggregationRound, JobCountStatistics, WitnessGeneratorJobMetadata, WitnessJobInfo,
};
use zksync_types::zkevm_test_harness::abstract_zksync_circuit::concrete_circuits::ZkSyncCircuit;
use zksync_types::zkevm_test_harness::abstract_zksync_circuit::concrete_circuits::ZkSyncProof;
use zksync_types::zkevm_test_harness::bellman::bn256::Bn256;
use zksync_types::zkevm_test_harness::bellman::plonk::better_better_cs::proof::Proof;
use zksync_types::zkevm_test_harness::witness::oracle::VmWitnessOracle;
use zksync_types::L1BatchNumber;

use crate::time_utils::{duration_to_naive_time, pg_interval_from_duration};
use crate::StorageProcessor;

#[derive(Debug)]
pub struct WitnessGeneratorDal<'a, 'c> {
    pub storage: &'a mut StorageProcessor<'c>,
}

impl WitnessGeneratorDal<'_, '_> {
    pub fn get_next_basic_circuit_witness_job(
        &mut self,
        processing_timeout: Duration,
        max_attempts: u32,
        last_l1_batch_to_process: u32,
    ) -> Option<WitnessGeneratorJobMetadata> {
        async_std::task::block_on(async {
            let processing_timeout = pg_interval_from_duration(processing_timeout);
            let result: Option<WitnessGeneratorJobMetadata> = sqlx::query!(
                "
                UPDATE witness_inputs
                SET status = 'in_progress', attempts = attempts + 1,
                    updated_at = now(), processing_started_at = now()
                WHERE l1_batch_number = (
                    SELECT l1_batch_number
                    FROM witness_inputs
                    WHERE l1_batch_number <= $3
                    AND
                    (   status = 'queued'
                        OR (status = 'in_progress' AND processing_started_at < now() - $1::interval)
                        OR (status = 'failed' AND attempts < $2)
                    )
                    ORDER BY l1_batch_number ASC
                    LIMIT 1
                    FOR UPDATE
                    SKIP LOCKED
                )
                RETURNING witness_inputs.*
               ",
                &processing_timeout,
                max_attempts as i32,
                last_l1_batch_to_process as i64
            )
            .fetch_optional(self.storage.conn())
            .await
            .unwrap()
            .map(|row| WitnessGeneratorJobMetadata {
                block_number: L1BatchNumber(row.l1_batch_number as u32),
                proofs: vec![],
            });

            result
        })
    }

    pub fn get_witness_generated_l1_batches(&mut self) -> Vec<(L1BatchNumber, AggregationRound)> {
        [
            "node_aggregation_witness_jobs",
            "leaf_aggregation_witness_jobs",
            "scheduler_witness_jobs",
            "witness_inputs",
        ]
        .map(|round| {
            async_std::task::block_on(async {
                let record = sqlx::query(&format!(
                    "SELECT MAX(l1_batch_number) as l1_batch FROM {} WHERE status='successful'",
                    round
                ))
                .fetch_one(self.storage.conn())
                .await
                .unwrap();
                (
                    L1BatchNumber(
                        record
                            .get::<Option<i64>, &str>("l1_batch")
                            .unwrap_or_default() as u32,
                    ),
                    match round {
                        "node_aggregation_witness_jobs" => AggregationRound::NodeAggregation,
                        "leaf_aggregation_witness_jobs" => AggregationRound::LeafAggregation,
                        "scheduler_witness_jobs" => AggregationRound::Scheduler,
                        "witness_inputs" => AggregationRound::BasicCircuits,
                        _ => unreachable!(),
                    },
                )
            })
        })
        .to_vec()
    }

    pub fn get_next_leaf_aggregation_witness_job(
        &mut self,
        processing_timeout: Duration,
        max_attempts: u32,
        last_l1_batch_to_process: u32,
    ) -> Option<WitnessGeneratorJobMetadata> {
        async_std::task::block_on(async {
            let processing_timeout = pg_interval_from_duration(processing_timeout);
            sqlx::query!(
                "
                UPDATE leaf_aggregation_witness_jobs
                SET status = 'in_progress', attempts = attempts + 1,
                    updated_at = now(), processing_started_at = now()
                WHERE l1_batch_number = (
                    SELECT l1_batch_number
                    FROM leaf_aggregation_witness_jobs
                    WHERE l1_batch_number <= $3
                    AND
                    (   status = 'queued'
                        OR (status = 'in_progress' AND processing_started_at < now() - $1::interval)
                        OR (status = 'failed' AND attempts < $2)
                    )
                    ORDER BY l1_batch_number ASC
                    LIMIT 1
                    FOR UPDATE
                    SKIP LOCKED
                )
                RETURNING leaf_aggregation_witness_jobs.*
                ", &processing_timeout,
                max_attempts as i32,
                last_l1_batch_to_process as i64
            )
                .fetch_optional(self.storage.conn())
                .await
                .unwrap()
                .map(|row| {
                    let l1_batch_number = L1BatchNumber(row.l1_batch_number as u32);
                    let number_of_basic_circuits = row.number_of_basic_circuits;

                    // Now that we have a job in `queued` status, we need to enrich it with the computed proofs.
                    // We select `aggregation_round = 0` to only get basic circuits.
                    // Note that at this point there cannot be any other circuits anyway,
                    // but we keep the check for explicitness
                    let basic_circuits_proofs: Vec<
                        Proof<Bn256, ZkSyncCircuit<Bn256, VmWitnessOracle<Bn256>>>,
                    > = self.load_proofs_for_block(l1_batch_number, AggregationRound::BasicCircuits);

                    assert_eq!(
                        basic_circuits_proofs.len(),
                        number_of_basic_circuits as usize,
                        "leaf_aggregation_witness_job for l1 batch {} is in status `queued`, but there are only {} computed basic proofs, which is different from expected {}",
                        l1_batch_number,
                        basic_circuits_proofs.len(),
                        number_of_basic_circuits
                    );

                    WitnessGeneratorJobMetadata {
                        block_number: l1_batch_number,
                        proofs: basic_circuits_proofs
                    }
                })
        })
    }

    pub fn get_next_node_aggregation_witness_job(
        &mut self,
        processing_timeout: Duration,
        max_attempts: u32,
        last_l1_batch_to_process: u32,
    ) -> Option<WitnessGeneratorJobMetadata> {
        async_std::task::block_on(async {
            let processing_timeout = pg_interval_from_duration(processing_timeout);
            sqlx::query!(
                "
                UPDATE node_aggregation_witness_jobs
                SET status = 'in_progress', attempts = attempts + 1,
                    updated_at = now(), processing_started_at = now()
                WHERE l1_batch_number = (
                    SELECT l1_batch_number
                    FROM node_aggregation_witness_jobs
                    WHERE l1_batch_number <= $3
                    AND
                    (   status = 'queued'
                        OR (status = 'in_progress' AND processing_started_at < now() - $1::interval)
                        OR (status = 'failed' AND attempts < $2)
                    )
                    ORDER BY l1_batch_number ASC
                    LIMIT 1
                    FOR UPDATE
                    SKIP LOCKED
                )
                RETURNING node_aggregation_witness_jobs.*
            ", &processing_timeout, 
                max_attempts as i32,
                last_l1_batch_to_process as i64,
            )
                .fetch_optional(self.storage.conn())
                .await
                .unwrap()
                .map(|row| {
                    let l1_batch_number = L1BatchNumber(row.l1_batch_number as u32);
                    let number_of_leaf_circuits = row.number_of_leaf_circuits.expect("number_of_leaf_circuits is not found in a `queued` `node_aggregation_witness_jobs` job");

                    // Now that we have a job in `queued` status, we need to enrich it with the computed proofs.
                    // We select `aggregation_round = 1` to only get leaf aggregation circuits
                    let leaf_circuits_proofs: Vec<
                        Proof<Bn256, ZkSyncCircuit<Bn256, VmWitnessOracle<Bn256>>>,
                    > = self.load_proofs_for_block(l1_batch_number, AggregationRound::LeafAggregation);

                    assert_eq!(
                        leaf_circuits_proofs.len(),
                        number_of_leaf_circuits as usize,
                        "node_aggregation_witness_job for l1 batch {} is in status `queued`, but there are only {} computed leaf proofs, which is different from expected {}",
                        l1_batch_number,
                        leaf_circuits_proofs.len(),
                        number_of_leaf_circuits
                    );
                    WitnessGeneratorJobMetadata {
                        block_number: l1_batch_number,
                        proofs: leaf_circuits_proofs
                    }
                })
        })
    }

    pub fn get_next_scheduler_witness_job(
        &mut self,
        processing_timeout: Duration,
        max_attempts: u32,
        last_l1_batch_to_process: u32,
    ) -> Option<WitnessGeneratorJobMetadata> {
        async_std::task::block_on(async {
            let processing_timeout = pg_interval_from_duration(processing_timeout);
            sqlx::query!(
                "
                UPDATE scheduler_witness_jobs
                SET status = 'in_progress', attempts = attempts + 1,
                    updated_at = now(), processing_started_at = now()
                WHERE l1_batch_number = (
                    SELECT l1_batch_number
                    FROM scheduler_witness_jobs
                    WHERE l1_batch_number <= $3
                    AND
                    (   status = 'queued'
                        OR (status = 'in_progress' AND processing_started_at < now() - $1::interval)
                        OR (status = 'failed' AND attempts < $2)
                    )
                    ORDER BY l1_batch_number ASC
                    LIMIT 1
                    FOR UPDATE
                    SKIP LOCKED
                )
                RETURNING scheduler_witness_jobs.*
                ", &processing_timeout, 
                max_attempts as i32,
                last_l1_batch_to_process as i64
            )
                .fetch_optional(self.storage.conn())
                .await
                .unwrap()
                .map(|row| {
                    let l1_batch_number = L1BatchNumber(row.l1_batch_number as u32);
                    // Now that we have a job in `queued` status, we need to enrich it with the computed proof.
                    // We select `aggregation_round = 2` to only get node aggregation circuits
                    let leaf_circuits_proofs: Vec<
                        Proof<Bn256, ZkSyncCircuit<Bn256, VmWitnessOracle<Bn256>>>,
                    > = self.load_proofs_for_block(l1_batch_number, AggregationRound::NodeAggregation);

                    assert_eq!(
                        leaf_circuits_proofs.len(),
                        1usize,
                        "scheduler_job for l1 batch {} is in status `queued`, but there is {} computed node proofs. We expect exactly one node proof.",
                        l1_batch_number.0,
                        leaf_circuits_proofs.len()
                    );

                    WitnessGeneratorJobMetadata {
                        block_number: l1_batch_number,
                        proofs: leaf_circuits_proofs
                    }
                })
        })
    }

    fn load_proofs_for_block(
        &mut self,
        block_number: L1BatchNumber,
        aggregation_round: AggregationRound,
    ) -> Vec<Proof<Bn256, ZkSyncCircuit<Bn256, VmWitnessOracle<Bn256>>>> {
        async_std::task::block_on(async {
            sqlx::query!(
                        "
                        SELECT circuit_type, result from prover_jobs
                        WHERE l1_batch_number = $1 AND status = 'successful' AND aggregation_round = $2
                        ORDER BY sequence_number ASC;
                        ",
                        block_number.0 as i64,
                        aggregation_round as i64
                )
                .fetch_all(self.storage.conn())
                .await
                .unwrap()
                .into_iter()
                .map(|row| {
                    ZkSyncProof::into_proof(bincode::deserialize::<ZkSyncProof<Bn256>>(
                        &row.result
                            .expect("prove_job with `successful` status has no result"),
                    )
                        .expect("cannot deserialize proof"))
                })
                .collect::<Vec<Proof<Bn256, ZkSyncCircuit<Bn256, VmWitnessOracle<Bn256>>>>>()
        })
    }

    pub fn mark_witness_job_as_successful(
        &mut self,
        block_number: L1BatchNumber,
        aggregation_round: AggregationRound,
        time_taken: Duration,
    ) {
        async_std::task::block_on(async {
            let table_name = Self::input_table_name_for(aggregation_round);
            let sql = format!(
                "UPDATE {}
                     SET status = 'successful', updated_at = now(), time_taken = $1
                     WHERE l1_batch_number = $2",
                table_name
            );
            let mut query = sqlx::query(&sql);
            query = query.bind(duration_to_naive_time(time_taken));
            query = query.bind(block_number.0 as i64);

            query.execute(self.storage.conn()).await.unwrap();
        });
    }

    /// Is invoked by the prover when all the required proofs are computed
    pub fn mark_witness_job_as_queued(
        &mut self,
        block_number: L1BatchNumber,
        aggregation_round: AggregationRound,
    ) {
        async_std::task::block_on(async {
            let table_name = Self::input_table_name_for(aggregation_round);
            let sql = format!(
                "UPDATE {}
                     SET status = 'queued', updated_at = now()
                     WHERE l1_batch_number = $1",
                table_name
            );
            let mut query = sqlx::query(&sql);
            query = query.bind(block_number.0 as i64);

            query.execute(self.storage.conn()).await.unwrap();
        });
    }

    pub fn mark_witness_job_as_skipped(
        &mut self,
        block_number: L1BatchNumber,
        aggregation_round: AggregationRound,
    ) {
        async_std::task::block_on(async {
            let table_name = Self::input_table_name_for(aggregation_round);
            let sql = format!(
                "UPDATE {}
                     SET status = 'skipped', updated_at = now()
                     WHERE l1_batch_number = $1",
                table_name
            );
            let mut query = sqlx::query(&sql);
            query = query.bind(block_number.0 as i64);

            let mut transaction = self.storage.start_transaction().await;
            query.execute(transaction.conn()).await.unwrap();

            transaction
                .blocks_dal()
                .set_skip_proof_for_l1_batch(block_number);
            transaction.commit().await;
        });
    }

    /// Is invoked by the Witness Generator when the previous aggregation round is complete
    pub fn mark_witness_job_as_waiting_for_proofs(
        &mut self,
        block_number: L1BatchNumber,
        aggregation_round: AggregationRound,
    ) {
        async_std::task::block_on(async {
            let table_name = Self::input_table_name_for(aggregation_round);
            let sql = format!(
                "UPDATE {}
                     SET status = 'waiting_for_proofs', updated_at = now()
                     WHERE l1_batch_number = $1",
                table_name
            );
            let mut query = sqlx::query(&sql);
            query = query.bind(block_number.0 as i64);

            query.execute(self.storage.conn()).await.unwrap();
        });
    }

    pub fn mark_witness_job_as_failed(
        &mut self,
        block_number: L1BatchNumber,
        aggregation_round: AggregationRound,
        time_taken: Duration,
        error: String,
        max_attempts: u32,
    ) {
        async_std::task::block_on(async {
            let table_name = Self::input_table_name_for(aggregation_round);
            let sql = format!(
                "UPDATE {}
                    SET status = 'failed', updated_at = now(), time_taken = $1, error = $2
                    WHERE l1_batch_number = $3
                RETURNING attempts
                ",
                table_name
            );
            let mut query = sqlx::query(&sql);
            query = query.bind(duration_to_naive_time(time_taken));
            query = query.bind(error);
            query = query.bind(block_number.0 as i64);

            let mut transaction = self.storage.start_transaction().await;
            let attempts = query
                .fetch_one(transaction.conn())
                .await
                .unwrap()
                .get::<i32, &str>("attempts");
            if attempts as u32 >= max_attempts {
                transaction
                    .blocks_dal()
                    .set_skip_proof_for_l1_batch(block_number);
            }
            transaction.commit().await;
        })
    }

    /// Creates a leaf_aggregation_job in `waiting_for_proofs` status,
    /// and also a node_aggregation_job and scheduler_job in `waiting_for_artifacts` status.
    /// The jobs will be advanced to `waiting_for_proofs` by the `Witness Generator` when the corresponding artifacts are computed,
    /// and to `queued` by the `Prover` when all the dependency proofs are computed
    pub fn create_aggregation_jobs(
        &mut self,
        block_number: L1BatchNumber,
        number_of_basic_circuits: usize,
    ) {
        async_std::task::block_on(async {
            let started_at = Instant::now();

            sqlx::query!(
                    "
                    INSERT INTO leaf_aggregation_witness_jobs
                        (l1_batch_number, basic_circuits, basic_circuits_inputs, basic_circuits_blob_url, basic_circuits_inputs_blob_url, number_of_basic_circuits, status, created_at, updated_at)
                    VALUES ($1, $2, $3, $4, $5, $6, 'waiting_for_proofs', now(), now())
                    ",
                    block_number.0 as i64,
                    vec![],
                    vec![],
                    basic_circuits_blob_url(block_number),
                    basic_circuits_inputs_blob_url(block_number),
                    number_of_basic_circuits as i64,
                )
                .execute(self.storage.conn())
                .await
                .unwrap();

            sqlx::query!(
                "
                    INSERT INTO node_aggregation_witness_jobs
                        (l1_batch_number, status, created_at, updated_at)
                    VALUES ($1, 'waiting_for_artifacts', now(), now())
                    ",
                block_number.0 as i64,
            )
            .execute(self.storage.conn())
            .await
            .unwrap();

            sqlx::query!(
                "
                    INSERT INTO scheduler_witness_jobs
                        (l1_batch_number, scheduler_witness, scheduler_witness_blob_url, status, created_at, updated_at)
                    VALUES ($1, $2, $3, 'waiting_for_artifacts', now(), now())
                    ",
                block_number.0 as i64,
                vec![],
                scheduler_witness_blob_url(block_number),
            )
            .execute(self.storage.conn())
            .await
            .unwrap();

            metrics::histogram!("dal.request", started_at.elapsed(), "method" => "create_aggregation_jobs");
        })
    }

    /// Saves artifacts in node_aggregation_job
    /// and advances it to `waiting_for_proofs` status
    /// it will be advanced to `queued` by the prover when all the dependency proofs are computed.
    /// If the node aggregation job was already `queued` in case of connrecunt run of same leaf aggregation job
    /// we keep the status as is to prevent data race.
    pub fn save_leaf_aggregation_artifacts(
        &mut self,
        block_number: L1BatchNumber,
        number_of_leaf_circuits: usize,
    ) {
        async_std::task::block_on(async {
            let started_at = Instant::now();
            sqlx::query!(
                "
                    UPDATE node_aggregation_witness_jobs
                        SET number_of_leaf_circuits = $1,
                            leaf_layer_subqueues_blob_url = $3,
                            aggregation_outputs_blob_url = $4,
                            status = 'waiting_for_proofs',
                            updated_at = now()
                    WHERE l1_batch_number = $2 AND status != 'queued'
                    ",
                number_of_leaf_circuits as i64,
                block_number.0 as i64,
                leaf_layer_subqueues_blob_url(block_number),
                aggregation_outputs_blob_url(block_number),
            )
            .execute(self.storage.conn())
            .await
            .unwrap();

            metrics::histogram!("dal.request", started_at.elapsed(), "method" => "save_leaf_aggregation_artifacts");
        })
    }

    /// Saves artifacts in scheduler_artifacts_jobs`
    /// and advances it to `waiting_for_proofs` status
    /// it will be advanced to `queued` by the prover when all the dependency proofs are computed.
    /// If the scheduler witness job was already `queued` in case of connrecunt run of same node aggregation job
    /// we keep the status as is to prevent data race.
    pub fn save_node_aggregation_artifacts(&mut self, block_number: L1BatchNumber) {
        async_std::task::block_on(async {
            let started_at = Instant::now();
            sqlx::query!(
                "
                    UPDATE scheduler_witness_jobs
                        SET final_node_aggregations_blob_url = $2,
                         status = 'waiting_for_proofs',
                         updated_at = now()
                    WHERE l1_batch_number = $1 AND status != 'queued'
                    ",
                block_number.0 as i64,
                final_node_aggregations_blob_url(block_number),
            )
            .execute(self.storage.conn())
            .await
            .unwrap();

            metrics::histogram!("dal.request", started_at.elapsed(), "method" => "save_node_aggregation_artifacts");
        })
    }

    pub fn save_final_aggregation_result(
        &mut self,
        block_number: L1BatchNumber,
        aggregation_result_coords: [[u8; 32]; 4],
    ) {
        async_std::task::block_on(async {
            let aggregation_result_coords_serialized =
                bincode::serialize(&aggregation_result_coords)
                    .expect("cannot serialize aggregation_result_coords");
            sqlx::query!(
                "
                    UPDATE scheduler_witness_jobs
                        SET aggregation_result_coords = $1,
                            updated_at = now()
                    WHERE l1_batch_number = $2
                    ",
                aggregation_result_coords_serialized,
                block_number.0 as i64,
            )
            .execute(self.storage.conn())
            .await
            .unwrap();
        })
    }

    pub fn get_witness_jobs_stats(
        &mut self,
        aggregation_round: AggregationRound,
    ) -> JobCountStatistics {
        async_std::task::block_on(async {
            let table_name = Self::input_table_name_for(aggregation_round);
            let sql = format!(
                r#"
                SELECT COUNT(*) as "count", status as "status"
                FROM {}
                GROUP BY status
                "#,
                table_name
            );
            let mut results: HashMap<String, i64> = sqlx::query(&sql)
                .fetch_all(self.storage.conn())
                .await
                .unwrap()
                .into_iter()
                .map(|row| (row.get("status"), row.get::<i64, &str>("count")))
                .collect::<HashMap<String, i64>>();

            JobCountStatistics {
                queued: results.remove("queued").unwrap_or(0i64) as usize,
                in_progress: results.remove("in_progress").unwrap_or(0i64) as usize,
                failed: results.remove("failed").unwrap_or(0i64) as usize,
                successful: results.remove("successful").unwrap_or(0i64) as usize,
            }
        })
    }

    pub fn required_proofs_count(
        &mut self,
        block_number: L1BatchNumber,
        aggregation_round: AggregationRound,
    ) -> usize {
        async_std::task::block_on(async {
            let table_name = Self::input_table_name_for(aggregation_round);
            let circuits_number_input_name = match aggregation_round {
                // Basic circuit job doesn't have any pre-requirements
                AggregationRound::BasicCircuits => unreachable!(),
                AggregationRound::LeafAggregation => "number_of_basic_circuits",
                AggregationRound::NodeAggregation => "number_of_leaf_circuits",
                // There is always just one final node circuit
                AggregationRound::Scheduler => return 1,
            };
            let sql = format!(
                r#"
                    SELECT {} as "count"
                    FROM {}
                    WHERE l1_batch_number = $1
                    "#,
                circuits_number_input_name, table_name
            );
            let mut query = sqlx::query(&sql);
            query = query.bind(block_number.0 as i64);
            query
                .fetch_one(self.storage.conn())
                .await
                .unwrap()
                .get::<i32, &str>("count") as usize
        })
    }

    fn input_table_name_for(aggregation_round: AggregationRound) -> &'static str {
        match aggregation_round {
            AggregationRound::BasicCircuits => "witness_inputs",
            AggregationRound::LeafAggregation => "leaf_aggregation_witness_jobs",
            AggregationRound::NodeAggregation => "node_aggregation_witness_jobs",
            AggregationRound::Scheduler => "scheduler_witness_jobs",
        }
    }

    pub fn get_jobs(
        &mut self,
        opts: GetWitnessJobsParams,
    ) -> Result<Vec<WitnessJobInfo>, sqlx::Error> {
        struct SqlSlice {
            columns: String,
            table_name: String,
        }

        impl SqlSlice {
            fn new(ar: u32, table_name: String) -> SqlSlice {
                SqlSlice {
                    columns: format!(
                        "{} as aggregation_round, 
                        l1_batch_number, 
                        created_at, 
                        updated_at, 
                        status, 
                        time_taken, 
                        processing_started_at, 
                        error, 
                        attempts",
                        ar
                    ),
                    table_name,
                }
            }

            fn sql(&self, opts: &GetWitnessJobsParams) -> String {
                let where_blocks = opts
                    .blocks
                    .as_ref()
                    .map(|b| format!("AND l1_batch_number BETWEEN {} AND {}", b.start, b.end))
                    .unwrap_or_default();

                format!(
                    "SELECT {} 
                    FROM {} 
                    WHERE 1 = 1 -- Where clause can't be empty
                    {where_blocks}",
                    self.columns, self.table_name
                )
            }
        }

        let slices = vec![
            SqlSlice::new(0, "witness_inputs".to_string()),
            SqlSlice::new(1, "leaf_aggregation_witness_jobs".to_string()),
            SqlSlice::new(2, "node_aggregation_witness_jobs".to_string()),
            SqlSlice::new(3, "scheduler_witness_jobs".to_string()),
        ];

        let sql = slices.iter().map(move |x| x.sql(&opts)).join(" UNION ");

        let query = sqlx::query_as(&sql);

        let x =
            async_std::task::block_on(async move { query.fetch_all(self.storage.conn()).await });

        Ok(x?
            .into_iter()
            .map(|x: StorageWitnessJobInfo| x.into())
            .collect())
    }

    pub fn save_witness_inputs(&mut self, block_number: L1BatchNumber) {
        async_std::task::block_on(async {
            sqlx::query!(
                "INSERT INTO witness_inputs(l1_batch_number, merkle_tree_paths, merkel_tree_paths_blob_url, status, created_at, updated_at) \
                 VALUES ($1, $2, $3, 'queued', now(), now())
                 ON CONFLICT (l1_batch_number) DO NOTHING",
                block_number.0 as i64,
                vec![],
                merkle_tree_paths_blob_url(block_number),
            )
                .fetch_optional(self.storage.conn())
                .await
                .unwrap();
        })
    }

    pub fn get_basic_circuit_and_circuit_inputs_blob_urls_to_be_cleaned(
        &mut self,
        limit: u8,
    ) -> Vec<(i64, (String, String))> {
        async_std::task::block_on(async {
            let job_ids = sqlx::query!(
                r#"
                    SELECT l1_batch_number, basic_circuits_blob_url, basic_circuits_inputs_blob_url FROM leaf_aggregation_witness_jobs
                    WHERE status='successful' AND is_blob_cleaned=FALSE
                    AND basic_circuits_blob_url is NOT NULL
                    AND basic_circuits_inputs_blob_url is NOT NULL
                    AND updated_at < NOW() - INTERVAL '30 days'
                    LIMIT $1;
                "#,
                limit as i32
            )
                .fetch_all(self.storage.conn())
                .await
                .unwrap();
            job_ids
                .into_iter()
                .map(|row| {
                    (
                        row.l1_batch_number,
                        (
                            row.basic_circuits_blob_url.unwrap(),
                            row.basic_circuits_inputs_blob_url.unwrap(),
                        ),
                    )
                })
                .collect()
        })
    }

    pub fn get_leaf_layer_subqueues_and_aggregation_outputs_blob_urls_to_be_cleaned(
        &mut self,
        limit: u8,
    ) -> Vec<(i64, (String, String))> {
        async_std::task::block_on(async {
            let job_ids = sqlx::query!(
                r#"
                    SELECT l1_batch_number, leaf_layer_subqueues_blob_url, aggregation_outputs_blob_url FROM node_aggregation_witness_jobs
                    WHERE status='successful' AND is_blob_cleaned=FALSE
                    AND leaf_layer_subqueues_blob_url is NOT NULL
                    AND aggregation_outputs_blob_url is NOT NULL
                    AND updated_at < NOW() - INTERVAL '30 days'
                    LIMIT $1;
                "#,
                limit as i32
            )
                .fetch_all(self.storage.conn())
                .await
                .unwrap();
            job_ids
                .into_iter()
                .map(|row| {
                    (
                        row.l1_batch_number,
                        (
                            row.leaf_layer_subqueues_blob_url.unwrap(),
                            row.aggregation_outputs_blob_url.unwrap(),
                        ),
                    )
                })
                .collect()
        })
    }

    pub fn get_scheduler_witness_and_node_aggregations_blob_urls_to_be_cleaned(
        &mut self,
        limit: u8,
    ) -> Vec<(i64, (String, String))> {
        async_std::task::block_on(async {
            let job_ids = sqlx::query!(
                r#"
                    SELECT l1_batch_number, scheduler_witness_blob_url, final_node_aggregations_blob_url FROM scheduler_witness_jobs
                    WHERE status='successful' AND is_blob_cleaned=FALSE
                    AND updated_at < NOW() - INTERVAL '30 days'
                    AND scheduler_witness_blob_url is NOT NULL
                    AND final_node_aggregations_blob_url is NOT NULL
                    LIMIT $1;
                "#,
                limit as i32
            )
                .fetch_all(self.storage.conn())
                .await
                .unwrap();
            job_ids
                .into_iter()
                .map(|row| {
                    (
                        row.l1_batch_number,
                        (
                            row.scheduler_witness_blob_url.unwrap(),
                            row.final_node_aggregations_blob_url.unwrap(),
                        ),
                    )
                })
                .collect()
        })
    }

    pub fn mark_leaf_aggregation_gcs_blobs_as_cleaned(&mut self, l1_batch_numbers: Vec<i64>) {
        async_std::task::block_on(async {
            sqlx::query!(
                r#"
                UPDATE leaf_aggregation_witness_jobs
                SET is_blob_cleaned=TRUE
                WHERE l1_batch_number = ANY($1);
            "#,
                &l1_batch_numbers[..]
            )
            .execute(self.storage.conn())
            .await
            .unwrap();
        })
    }

    pub fn mark_node_aggregation_gcs_blobs_as_cleaned(&mut self, l1_batch_numbers: Vec<i64>) {
        async_std::task::block_on(async {
            sqlx::query!(
                r#"
                UPDATE node_aggregation_witness_jobs
                SET is_blob_cleaned=TRUE
                WHERE l1_batch_number = ANY($1);
            "#,
                &l1_batch_numbers[..]
            )
            .execute(self.storage.conn())
            .await
            .unwrap();
        })
    }

    pub fn mark_scheduler_witness_gcs_blobs_as_cleaned(&mut self, l1_batch_numbers: Vec<i64>) {
        async_std::task::block_on(async {
            sqlx::query!(
                r#"
                UPDATE scheduler_witness_jobs
                SET is_blob_cleaned=TRUE
                WHERE l1_batch_number = ANY($1);
            "#,
                &l1_batch_numbers[..]
            )
            .execute(self.storage.conn())
            .await
            .unwrap();
        })
    }
}

pub struct GetWitnessJobsParams {
    pub blocks: Option<Range<L1BatchNumber>>,
}
