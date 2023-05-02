extern crate pretty_env_logger;
#[macro_use]
extern crate log;

use std::collections::HashMap;
use std::fs::File;
use std::io::prelude::*;
use std::thread;

use tinyrand::{Rand, RandRange, Seeded, StdRand};
use tinyrand_std::clock_seed::ClockSeed;

use clap::Parser;
use rust_lapper::Lapper;
use serde_json::json;

mod cli;
mod io;

const NOVLMAGIC: u64 = 10000;
/* When performing novl randomization, we break the uncovered spans of
*  the genome into pieces and shuffle them along with the intervals.
*  Truly random novl would break all the uncovered spans into 1bp pieces.
*  However, this is extremly inefficient. Instead, we break it into pieces
*  randomly between 1bp and some maximum size. If this maximum size was
*  the length of all uncovered spans, we could end up making one giant
*  uncovered span and then grouping all the intervals end-to-end.
*  By setting the maximum size to some percent of the uncovered span
*  we lower the bias towards making large gaps.
*  NOVLMAGIC is calculated as int(1 / max_size_percent)
*/

// ***********
// Randomizers
// ***********
fn shuffle_intervals(
    intv: &Lapper<u64, u64>,
    genome: &io::GenomeShift,
    per_chrom: bool,
) -> Lapper<u64, u64> {
    /*
        Randomly move each interval to new position
    */
    let mut ret = Vec::<io::Iv>::new();
    let mut rand = StdRand::seed(ClockSeed::default().next_u64());

    for i in intv.iter() {
        let (lower, upper) = if per_chrom {
            match genome.chrom.find(i.start, i.stop).next() {
                Some(b) => (b.start, b.stop),
                None => panic!("Interval @ ({}, {}) not hitting genome", i.start, i.stop),
            }
        } else {
            (0, genome.span)
        };
        let shift = rand.next_range(lower..(upper - (i.stop - i.start)));
        ret.push(io::Iv {
            start: i.start + shift,
            stop: i.stop + shift,
            val: 0,
        });
    }
    Lapper::<u64, u64>::new(ret)
}

fn circle_intervals(
    intv: &Lapper<u64, u64>,
    genome: &io::GenomeShift,
    per_chrom: bool,
) -> Lapper<u64, u64> {
    /*
        Randomly shift all intervals downstream with wrap-around
    */
    let mut rand = StdRand::seed(ClockSeed::default().next_u64());
    let mut ret = Vec::<io::Iv>::new();

    let genome_shift: u64 = rand.next_range(0..(genome.span));

    for i in intv.iter() {
        let (lower, upper, shift) = if per_chrom {
            match genome.chrom.find(i.start, i.stop).next() {
                Some(b) => (b.start, b.stop, genome_shift % b.val),
                None => panic!("Interval @ ({}, {}) not hitting genome", i.start, i.stop),
            }
        } else {
            (0, genome.span, genome_shift)
        };

        let new_start: u64 = i.start + shift;
        let new_end: u64 = i.stop + shift;

        if new_start >= upper {
            ret.push(io::Iv {
                start: new_start - upper,
                stop: new_end - upper,
                val: 0,
            });
        } else if new_end > upper {
            ret.push(io::Iv {
                start: new_start,
                stop: upper,
                val: 0,
            });
            ret.push(io::Iv {
                start: lower,
                stop: new_end - upper,
                val: 0,
            });
        } else {
            ret.push(io::Iv {
                start: new_start,
                stop: new_end,
                val: 0,
            });
        }
    }
    Lapper::<u64, u64>::new(ret)
}

fn novl_intervals(
    intv: &Lapper<u64, u64>,
    genome: &io::GenomeShift,
    per_chrom: bool,
) -> Lapper<u64, u64> {
    /*
        Randomly move each interval to new position without overlapping them
    */
    let mut rand = StdRand::seed(ClockSeed::default().next_u64());
    let mut ret: Vec<io::Iv> = vec![];

    let spans = match per_chrom {
        true => genome.chrom.intervals.clone(),
        false => vec![io::Iv {
            start: 0,
            stop: genome.span,
            val: 0,
        }],
    };

    for subi in spans {
        let mut cur_intervals: Vec<(bool, u64)> = vec![];
        let mut m_gap: u64 = match &genome.gap_budget {
            Some(g) => g[&subi.start],
            None => panic!("How are you using the gap_budget without making it first?"),
        };
        //let mut num_gap_pieces = 0;
        while m_gap > 0 {
            let g_l = rand.next_range(1..std::cmp::max(2, m_gap / NOVLMAGIC));
            cur_intervals.push((false, g_l));
            m_gap -= g_l;
            //num_gap_pieces += 1;
        }
        //info!("gap pieces {}", num_gap_pieces);

        cur_intervals.extend(
            intv.find(subi.start, subi.stop)
                .map(|i| (true, i.stop - i.start)),
        );
        fastrand::shuffle(&mut cur_intervals);

        let mut cur_pos = subi.start;
        for i in cur_intervals {
            if i.0 {
                ret.push(io::Iv {
                    start: cur_pos,
                    stop: cur_pos + i.1,
                    val: 0,
                });
            }
            cur_pos += i.1;
        }
    }

    Lapper::<u64, u64>::new(ret)
}

// **********
// Overlapers
// **********
fn get_num_overlap_count(a_lap: &Lapper<u64, u64>, b_lap: &Lapper<u64, u64>) -> u64 {
    /* Return number of b intervals intersecting each of a's intervals */
    a_lap
        .iter()
        .map(|i| b_lap.find(i.start, i.stop).count() as u64)
        .sum()
}

fn get_any_overlap_count(a_lap: &Lapper<u64, u64>, b_lap: &Lapper<u64, u64>) -> u64 {
    /* Return number of a intervals intersecting b intervals */
    a_lap
        .iter()
        .map(|i| match b_lap.find(i.start, i.stop).next() {
            Some(_) => 1,
            None => 0,
        })
        .sum()
}

// *******
// Helpers
// *******
fn mean_std(v: &[u64]) -> (f64, f64) {
    let n = v.len() as f64;
    let mean = v.iter().sum::<u64>() as f64 / n;

    let variance = v.iter().map(|x| (*x as f64 - mean).powi(2)).sum::<f64>() / n;

    let std_dev = variance.sqrt();

    (mean, std_dev)
}

fn count_permutations(o_count: u64, obs: &Vec<u64>, alt: char) -> f64 {
    /*
        Return number of permutations the observed count is (g)reater or (l)ess than
    */
    let mut g_count: f64 = 0.0;
    if alt == 'l' {
        for i in obs {
            g_count += if o_count >= *i { 1.0 } else { 0.0 };
        }
    } else if alt == 'g' {
        for i in obs {
            g_count += if o_count <= *i { 1.0 } else { 0.0 };
        }
    }

    g_count
}

// Should probably go into an implementation of GenomeShift
fn make_gap_budget(
    genome: &io::GenomeShift,
    intervals: &Lapper<u64, u64>,
    per_chrom: &bool,
) -> HashMap<u64, u64> {
    info!("making gap budget");
    let mut ret = HashMap::<u64, u64>::new();
    match per_chrom {
        false => {
            ret.insert(0, genome.span - intervals.cov());
        }
        true => {
            for i in genome.chrom.iter() {
                ret.insert(
                    i.start,
                    intervals
                        .find(i.start, i.stop)
                        .map(|p| p.stop - p.start)
                        .sum(),
                );
            }
        }
    }
    ret
}

fn main() -> std::io::Result<()> {
    pretty_env_logger::formatted_timed_builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    // IO
    let args = cli::ArgParser::parse();
    if !cli::validate_args(&args) {
        error!("please fix arguments");
        std::process::exit(1);
    }

    let mask = args.mask.map(|p| io::read_mask(&p));
    let mut genome = io::read_genome(&args.genome, &mask);
    let mut a_lapper = io::read_bed(&args.bed_a, &genome, &mask);
    let mut b_lapper = io::read_bed(&args.bed_b, &genome, &mask);

    // Setup
    if !args.no_merge {
        info!("merging overlaps");
        a_lapper.merge_overlaps();
        b_lapper.merge_overlaps();
    }
    let swapped = match !args.no_swap & (a_lapper.len() > b_lapper.len()) {
        true => {
            info!("swapping A for shorter B");
            std::mem::swap(&mut a_lapper, &mut b_lapper);
            true
        }
        false => false,
    };
    let overlapper = match args.count {
        cli::Counter::Any => get_any_overlap_count,
        cli::Counter::All => get_num_overlap_count,
    };
    let randomizer = match args.random {
        cli::Randomizer::Circle => circle_intervals,
        cli::Randomizer::Shuffle => shuffle_intervals,
        cli::Randomizer::Novl => {
            genome.gap_budget = Some(make_gap_budget(&genome, &a_lapper, &args.per_chrom));
            novl_intervals
        }
    };

    // profiling
    /*let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(1000)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .unwrap();*/

    // Processing
    let initial_overlap_count: u64 = overlapper(&a_lapper, &b_lapper);
    info!("{} intersections", initial_overlap_count);

    let mut handles = Vec::new();
    let chunk_size: u32 = ((args.num_times as f32) / (args.threads as f32)).ceil() as u32;

    for i in 0..args.threads as u32 {
        let m_a = a_lapper.clone();
        let m_b = b_lapper.clone();
        let m_genome = genome.clone();
        // Send chunk to thread
        let start_iter = i * chunk_size;
        let stop_iter = std::cmp::min(start_iter + chunk_size, args.num_times);
        handles.push(thread::spawn(move || {
            (start_iter..stop_iter)
                .map(|_| overlapper(&randomizer(&m_a, &m_genome, args.per_chrom), &m_b))
                .collect()
        }));
    }

    // Collect
    let mut all_counts: Vec<u64> = vec![];
    for handle in handles {
        let result: Vec<u64> = handle.join().unwrap();
        all_counts.extend(result);
    }
    /*if let Ok(report) = guard.report().build() {
        println!("report: {:?}", &report);
    };*/
    // Calculate
    let (mu, sd) = mean_std(&all_counts);
    let alt = if (initial_overlap_count as f64) < mu {
        'l'
    } else {
        'g'
    };
    let g_count = count_permutations(initial_overlap_count, &all_counts, alt);
    let p_val = (g_count + 1.0) / ((all_counts.len() as f64) + 1.0);

    let mut z_score = 0.0;
    if (initial_overlap_count == 0) & (mu == 0.0) {
        warn!("z_score cannot be computed");
    } else {
        z_score = ((initial_overlap_count as f64) - mu) / sd;
    }

    // Output
    info!("perm mu: {}", mu);
    info!("perm sd: {}", sd);
    info!("alt : {}", alt);
    info!("p-val : {}", p_val);

    let data = json!({"pval": p_val,
                      "zscore": z_score,
                      "obs":initial_overlap_count,
                      "perm_mu": mu,
                      "perm_sd": sd,
                      "alt": alt,
                      "n": args.num_times,
                      "swapped": swapped,
                      "no_merge": args.no_merge,
                      "random": args.random as u8,
                      "counter": args.count as u8,
                      "A_cnt" : a_lapper.len(),
                      "B_cnt" : b_lapper.len(),
                      "per_chrom": args.per_chrom,
                      "perms": all_counts});
    let json_str = serde_json::to_string(&data).unwrap();

    let mut file = File::create(args.output)?;
    file.write_all(json_str.as_bytes())
}
