use async_sqlite::error::DatabaseError;
use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, Criterion, 
    Throughput, BenchmarkId, measurement::WallTime, PlotConfiguration, 
    AxisScale
};
use rand::prelude::*;
use std::alloc::System;
use std::sync::Arc;
use std::time::Duration;
use sysinfo::{System, SystemExt, ProcessExt, CpuExt};

use async_sqlite::spawn::SpawnBlocking;
use async_sqlite::{
    AsyncStdExecutor, CrossbeamExecutor, DatabaseConfig, ImmediateExecutor, SQLiteAdapter, SmolExecutor, TokioExecutor
};
use plotters::prelude::*;

// Structure to track resource usage
#[derive(Debug, Clone)]
struct ResourceMetrics {
    cpu_usage: f32,
    memory_usage: u64,
    timestamp: Duration,
}

// Resource monitor that can be shared between threads
struct ResourceMonitor {
    system: System,
    pid: sysinfo::Pid,
    metrics: Arc<Mutex<Vec<ResourceMetrics>>>,
    sampling_interval: Duration,
    is_running: Arc<AtomicBool>,
}

impl ResourceMonitor {
    fn new(sampling_interval: Duration) -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        
        let pid = sysinfo::get_current_pid()
            .expect("Failed to get current process ID");
        
        Self {
            system,
            pid,
            metrics: Arc::new(Mutex::new(Vec::new())),
            sampling_interval,
            is_running: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start_monitoring(&self) {
        let metrics = Arc::clone(&self.metrics);
        let is_running = Arc::clone(&self.is_running);
        let mut system = self.system.clone();
        let pid = self.pid;
        let interval = self.sampling_interval;

        is_running.store(true, Ordering::SeqCst);
        
        std::thread::spawn(move || {
            let start = Instant::now();
            while is_running.load(Ordering::SeqCst) {
                system.refresh_all();
                
                if let Some(process) = system.process(pid) {
                    let metric = ResourceMetrics {
                        cpu_usage: process.cpu_usage(),
                        memory_usage: process.memory(),
                        timestamp: start.elapsed(),
                    };
                    
                    metrics.lock().unwrap().push(metric);
                }
                
                std::thread::sleep(interval);
            }
        });
    }

    fn stop_monitoring(&self) {
        self.is_running.store(false, Ordering::SeqCst);
    }

    fn get_metrics(&self) -> Vec<ResourceMetrics> {
        self.metrics.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone)]
struct TestData {
    id: i64,
    name: String,
    value: f64,
    data: Vec<u8>,
}

impl TestData {
    fn random() -> Self {
        let mut rng = rand::thread_rng();
        Self {
            id: rng.gen(),
            name: rng.gen::<u64>().to_string(),
            value: rng.gen(),
            data: (0..rng.gen_range(100..1000)).map(|_| rng.gen()).collect(),
        }
    }
}

async fn setup_database<E>(config: &DatabaseConfig) -> SQLiteAdapter<E>
where
    E: SpawnBlocking,
{
    let db = SQLiteAdapter::<E>::new_threaded(config.clone())
        .await
        .expect("Failed to create database");

    // Create tables and indexes
    db.execute(|conn| {
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            
            CREATE TABLE IF NOT EXISTS test_data (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                value REAL NOT NULL,
                data BLOB NOT NULL
            );
            
            CREATE INDEX IF NOT EXISTS idx_name ON test_data(name);
            CREATE INDEX IF NOT EXISTS idx_value ON test_data(value);
            
            -- Clean up any existing data
            DELETE FROM test_data;
            
            -- Initialize with some seed data for query benchmarks
            WITH RECURSIVE 
            seq(n, value) AS (
                SELECT 1, 0.0
                UNION ALL
                SELECT n + 1, value + 0.1
                FROM seq
                WHERE n < 1000
            )
            INSERT INTO test_data (id, name, value, data)
            SELECT 
                n,
                'test_' || n,
                value,
                randomblob(100)
            FROM seq;
            "
        )?;
        Ok(())
    })
    .await
    .expect("Failed to initialize database");

    // Verify setup
    db.execute(|conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM test_data",
            [],
            |row| row.get(0),
        )?;
        assert!(count > 0, "Database should contain seed data");
        Ok(())
    })
    .await
    .expect("Failed to verify database setup");

    db
}

// Helper enum to identify executors in results
#[derive(Debug, Clone, Copy)]
enum ExecutorType {
    Tokio,
    Immediate,
    Crossbeam,
    AsyncStd,
    Smol,
}

impl std::fmt::Display for ExecutorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorType::Tokio => write!(f, "Tokio"),
            ExecutorType::Immediate => write!(f, "Immediate"),
            ExecutorType::Crossbeam => write!(f, "Crossbeam"),
            ExecutorType::AsyncStd => write!(f, "async-std"),
            ExecutorType::Smol => write!(f, "smol"),
        }
    }
}

pub fn benchmark(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let config = DatabaseConfig {
        path: ":memory:".into(),
        max_connections: 10,
        ..Default::default()
    };

    // Setup databases
    let db_tokio = runtime.block_on(setup_database::<TokioExecutor>(&config));
    let db_immediate = runtime.block_on(setup_database::<ImmediateExecutor>(&config));
    let db_crossbeam = runtime.block_on(setup_database::<CrossbeamExecutor>(&config));
    let db_async_std = runtime.block_on(setup_database::<AsyncStdExecutor>(&config));
    let db_smol = runtime.block_on(setup_database::<SmolExecutor>(&config));


    let test_data: Vec<TestData> = (0..1000).map(|_| TestData::random()).collect();
    let test_data = Arc::new(test_data);

    // Compare single inserts
    let mut single_insert = c.benchmark_group("Single Insert Comparison");
    single_insert.throughput(Throughput::Elements(1));
    single_insert.measurement_time(Duration::from_secs(10));
    single_insert.plot_config(PlotConfiguration::default()
        .summary_scale(AxisScale::Linear));

    for executor in [ExecutorType::Tokio, ExecutorType::Immediate, ExecutorType::Crossbeam, ExecutorType::AsyncStd, ExecutorType::Smol] {
        single_insert.bench_with_input(
            BenchmarkId::from_parameter(executor),
            &test_data,
            |b, data| {
                let data = data.clone();
                b.to_async(&runtime).iter(|| async {
              
                    match executor {
                        ExecutorType::Tokio => {
                            let data = data.clone();
                            db_tokio.execute(move |conn| {
                                let item = &data[0];
                                conn.execute(
                                    "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                    (item.id, &item.name, item.value, &item.data),
                                )?;
                                Ok(())
                            }).await
                        },
                        ExecutorType::Immediate => {
                            let data = data.clone();
                            db_immediate.execute(move |conn| {
                                let item = &data[0];
                                conn.execute(
                                    "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                    (item.id, &item.name, item.value, &item.data),
                                )?;
                                Ok(())
                            }).await
                        },
                        ExecutorType::Crossbeam => {
                            let data = data.clone();
                            db_crossbeam.execute(move |conn| {
                                let item = &data[0];
                                conn.execute(
                                    "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                    (item.id, &item.name, item.value, &item.data),
                                )?;
                                Ok(())
                            }).await
                        },
                        ExecutorType::AsyncStd => {
                            let data = data.clone();
                            db_async_std.execute(move |conn| {
                                let item = &data[0];
                                conn.execute(
                                    "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                    (item.id, &item.name, item.value, &item.data),
                                )?;
                                Ok(())
                            }).await
                        },
                        ExecutorType::Smol => {
                            let data = data.clone();
                            db_smol.execute(move |conn| {
                                let item = &data[0];
                                conn.execute(
                                    "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                    (item.id, &item.name, item.value, &item.data),
                                )?;
                                Ok(())
                            }).await
                        },
                    }
                })
            },
        );
    }
    single_insert.finish();

    // Compare batch operations
    let mut batch_ops = c.benchmark_group("Batch Operations Comparison");
    batch_ops.throughput(Throughput::Elements(100));
    batch_ops.measurement_time(Duration::from_secs(10));
    batch_ops.plot_config(PlotConfiguration::default()
        .summary_scale(AxisScale::Linear));

    for executor in [ExecutorType::Tokio, ExecutorType::Immediate, ExecutorType::Crossbeam, ExecutorType::AsyncStd, ExecutorType::Smol] {
        batch_ops.bench_with_input(
            BenchmarkId::from_parameter(executor),
            &test_data,
            |b, data| {
                let data = data.clone();
                b.to_async(&runtime).iter(|| async {
                    let items: Vec<TestData> = data.iter().take(100).cloned().collect();
                    match executor {
                        ExecutorType::Tokio => {
                            db_tokio.transaction(move |tx| {
                                for item in items {
                                    tx.execute(
                                        "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                        (item.id, &item.name, item.value, &item.data),
                                    )?;
                                }
                                Ok(())
                            }).await
                        },
                        ExecutorType::Immediate => {
                            db_immediate.transaction(move |tx| {
                                for item in items {
                                    tx.execute(
                                        "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                        (item.id, &item.name, item.value, &item.data),
                                    )?;
                                }
                                Ok(())
                            }).await
                        },
                        ExecutorType::Crossbeam => {
                            db_crossbeam.transaction(move |tx| {
                                for item in items {
                                    tx.execute(
                                        "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                        (item.id, &item.name, item.value, &item.data),
                                    )?;
                                }
                                Ok(())
                            }).await
                        },
                        ExecutorType::AsyncStd => {
                            db_async_std.transaction(move |tx| {
                                for item in items {
                                    tx.execute(
                                        "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                        (item.id, &item.name, item.value, &item.data),
                                    )?;
                                }
                                Ok(())
                            }).await
                        },
                        ExecutorType::Smol => {
                            db_smol.transaction(move |tx| {
                                for item in items {
                                    tx.execute(
                                        "INSERT INTO test_data (id, name, value, data) VALUES (?, ?, ?, ?)",
                                        (item.id, &item.name, item.value, &item.data),
                                    )?;
                                }
                                Ok(())
                            }).await
                        },
                    }
                })
            },
        );
    }
    batch_ops.finish();

  // Compare complex queries with fixed lifetime
  //let mut complex_query = c.benchmark_group("Complex Query Comparison");
  //complex_query.throughput(Throughput::Elements(1));
  //complex_query.measurement_time(Duration::from_secs(10));
  //complex_query.plot_config(PlotConfiguration::default()
  //    .summary_scale(AxisScale::Linear));
//
  //let query = String::from("
  //    WITH RECURSIVE hierarchy AS (
  //        SELECT id, name, value, 1 as level
  //        FROM test_data
  //        WHERE value > ?
  //        UNION ALL
  //        SELECT t.id, t.name, t.value, h.level + 1
  //        FROM test_data t
  //        JOIN hierarchy h ON t.value > h.value
  //        WHERE h.level < 3
  //    )
  //    SELECT id, name, value, level
  //    FROM hierarchy
  //    ORDER BY level, value DESC
  //    LIMIT 100
  //");
//
  //for executor in [ExecutorType::Tokio, ExecutorType::Immediate, ExecutorType::Crossbeam] {
  //  complex_query.bench_with_input(
  //      BenchmarkId::from_parameter(executor),
  //      &query,
  //      |b, query| {
  //          let query = query.clone();
  //          b.to_async(&runtime).iter(|| async {
  //              let query = query.clone();
  //              match executor {
  //                  ExecutorType::Tokio => {
  //                      db_tokio.execute(move |conn| {
  //                       
  //                          let mut stmt = conn.prepare(&query)?;
  //                          let rows = stmt.query_map([0.5], |row| {
  //                              Ok((
  //                                  row.get::<_, i64>(0)?,
  //                                  row.get::<_, String>(1)?,
  //                                  row.get::<_, f64>(2)?,
  //                                  row.get::<_, i32>(3)?,
  //                              ))
  //                          })?;
  //                          Ok(rows.collect::<Result<Vec<_>, _>>())
  //                      }).await
  //                  },
  //                  ExecutorType::Immediate => {
  //                      db_immediate.execute(move |conn| {
  //                         
  //                          let mut stmt = conn.prepare(&query)?;
  //                          let rows = stmt.query_map([0.5], |row| {
  //                              Ok((
  //                                  row.get::<_, i64>(0)?,
  //                                  row.get::<_, String>(1)?,
  //                                  row.get::<_, f64>(2)?,
  //                                  row.get::<_, i32>(3)?,
  //                              ))
  //                          })?;
  //                          Ok(rows.collect::<Result<Vec<_>, _>>())
  //                      }).await
  //                  },
  //                  ExecutorType::Crossbeam => {
  //                      db_crossbeam.execute(move |conn| {
  //                         
  //                          let mut stmt = conn.prepare(&query)?;
  //                          let rows = stmt.query_map([0.5], |row| {
  //                              Ok((
  //                                  row.get::<_, i64>(0)?,
  //                                  row.get::<_, String>(1)?,
  //                                  row.get::<_, f64>(2)?,
  //                                  row.get::<_, i32>(3)?,
  //                              ))
  //                          })?;
  //                          Ok(rows.collect::<Result<Vec<_>, _>>())
  //                      }).await
  //                  },
  //              }
  //          })
  //      },
  //  );
//}//
//co//mplex_query.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(50)
        .measurement_time(Duration::from_secs(10))
        .with_plots();
    targets = benchmark
}

criterion_main!(benches);

// Concurrent operations benchmark
pub async fn benchmark_concurrent_ops(
    c: &mut Criterion,
    runtime: &tokio::runtime::Runtime,
    dbs: &DatabaseInstances,
    test_data: Arc<Vec<TestData>>,
) {
    let mut group = c.benchmark_group("Concurrent Operations");
    group.throughput(Throughput::Elements(50));
    group.measurement_time(Duration::from_secs(15));
    group.plot_config(PlotConfiguration::default()
        .summary_scale(AxisScale::Linear));

    // Test different concurrency levels
    let concurrency_levels = [1, 5, 10, 20, 50];

    for &concurrency in &concurrency_levels {
        for executor in [
            ExecutorType::Tokio,
            ExecutorType::AsyncStd,
            ExecutorType::Smol,
            ExecutorType::Immediate,
            ExecutorType::Crossbeam,
        ] {
            let id = format!("{}_concurrent_{}", executor, concurrency);
            group.bench_with_input(
                BenchmarkId::new("mixed_operations", id),
                &test_data,
                |b, data| {
                    let data = data.clone();
                    let monitor = Arc::new(ResourceMonitor::new(Duration::from_millis(100)));
                    
                    b.to_async(runtime).iter(|| {
                        let data = data.clone();
                        let monitor = Arc::clone(&monitor);
                        async move {
                            monitor.start_monitoring();
                            
                            let mut tasks = FuturesUnordered::new();
                            
                            // Mix of different operations
                            for i in 0..concurrency {
                                let db = dbs.get_db(executor);
                                let chunk = data.iter()
                                    .skip(i * (data.len() / concurrency))
                                    .take(data.len() / concurrency)
                                    .cloned()
                                    .collect::<Vec<_>>();
                                
                                tasks.push(async move {
                                    // Mix of reads and writes
                                    let results = futures::join!(
                                        // Insert operation
                                        db.transaction(|tx| {
                                            for item in &chunk[..5] {
                                                tx.execute(
                                                    "INSERT INTO test_data (id, name, value, data) 
                                                     VALUES (?, ?, ?, ?)",
                                                    (item.id, &item.name, item.value, &item.data),
                                                )?;
                                            }
                                            Ok(())
                                        }),
                                        
                                        // Query operation
                                        db.execute(|conn| {
                                            let mut stmt = conn.prepare(
                                                "SELECT * FROM test_data 
                                                 WHERE value > ? 
                                                 ORDER BY value DESC 
                                                 LIMIT 10"
                                            )?;
                                            let rows = stmt.query_map([0.5], |row| {
                                                Ok((
                                                    row.get::<_, i64>(0)?,
                                                    row.get::<_, String>(1)?,
                                                    row.get::<_, f64>(2)?,
                                                ))
                                            })?;
                                            rows.collect::<Result<Vec<_>, _>>()
                                        }),
                                        
                                        // Update operation
                                        db.execute(|conn| {
                                            conn.execute(
                                                "UPDATE test_data 
                                                 SET value = value + 1 
                                                 WHERE id = ?",
                                                [chunk[0].id],
                                            )?;
                                            Ok(())
                                        })
                                    );
                                    
                                    results.0?;
                                    results.1?;
                                    results.2?;
                                    Ok::<_, DatabaseError>(())
                                });
                            }
                            
                            // Wait for all tasks to complete
                            while let Some(result) = tasks.next().await {
                                result?;
                            }
                            
                            monitor.stop_monitoring();
                            
                            // Plot resource usage
                            plot_resource_usage(
                                &monitor.get_metrics(),
                                &format!("resource_usage_{}_{}.png", executor, concurrency),
                            )?;
                            
                            Ok::<_, DatabaseError>(())
                        }
                    });
                },
            );
        }
    }
    
    group.finish();
}

// Plotting function for resource usage
fn plot_resource_usage(metrics: &[ResourceMetrics], filename: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = BitMapBackend::new(filename, (1024, 768)).into_drawing_area();
    root.fill(&WHITE)?;
    
    let (min_time, max_time) = metrics.iter()
        .map(|m| m.timestamp)
        .fold((Duration::from_secs(0), Duration::from_secs(0)), 
            |acc, x| (acc.0.min(x), acc.0.max(x)));
    
    let max_cpu = metrics.iter()
        .map(|m| m.cpu_usage)
        .fold(0.0, f32::max);
    
    let max_mem = metrics.iter()
        .map(|m| m.memory_usage)
        .fold(0, u64::max);

    let mut chart = ChartBuilder::on(&root)
        .caption("Resource Usage Over Time", ("sans-serif", 50).into_font())
        .margin(10)
        .x_label_area_size(40)
        .y_label_area_size(60)
        .right_y_label_area_size(60)
        .build_cartesian_2d(
            min_time.as_secs_f64()..max_time.as_secs_f64(),
            0f32..max_cpu,
        )?
        .set_secondary_coord(
            min_time.as_secs_f64()..max_time.as_secs_f64(),
            0.0..max_mem as f64,
        );

    chart
        .configure_mesh()
        .y_desc("CPU Usage (%)")
        .draw()?;

    chart
        .configure_secondary_axes()
        .y_desc("Memory Usage (bytes)")
        .draw()?;

    // Plot CPU usage
    chart.draw_series(LineSeries::new(
        metrics.iter().map(|m| (m.timestamp.as_secs_f64(), m.cpu_usage)),
        &RED,
    ))?.label("CPU Usage")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &RED));

    // Plot memory usage
    chart.draw_series(LineSeries::new(
        metrics.iter().map(|m| (m.timestamp.as_secs_f64(), m.memory_usage as f64)),
        &BLUE,
    ))?.label("Memory Usage")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &BLUE));

    chart.configure_series_labels()
        .background_style(&WHITE.mix(0.8))
        .border_style(&BLACK)
        .draw()?;

    Ok(())
}