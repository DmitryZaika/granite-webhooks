//! Shortest driving order for a day's stops. Port of
//! `app/utils/scheduleRouteOptimizer.ts`.
//!
//! Point 0 is the start (the company address); points 1..n are the stops.
//! `cost[from][to]` is the drive time between two points. The route is open:
//! it starts at point 0 and ends wherever the trip is shortest.

use chrono::{Duration, NaiveDateTime};

/// Up to this many stops are solved exactly; more use a heuristic.
const EXACT_SOLVE_LIMIT: usize = 12;

/// Total drive time of visiting `order` from point 0.
pub fn route_cost(cost: &[Vec<f64>], order: &[usize]) -> f64 {
    let mut previous = 0;
    let mut total = 0.0;
    for &point in order {
        total += cost[previous][point];
        previous = point;
    }
    total
}

fn solve_exact(cost: &[Vec<f64>], stops: usize) -> Vec<usize> {
    let full = (1usize << stops) - 1;
    let mut best = vec![vec![f64::INFINITY; stops]; full + 1];
    let mut parent = vec![vec![usize::MAX; stops]; full + 1];
    for stop in 0..stops {
        best[1 << stop][stop] = cost[0][stop + 1];
    }
    for mask in 1..=full {
        for last in 0..stops {
            let current = best[mask][last];
            if mask & (1 << last) == 0 || current.is_infinite() {
                continue;
            }
            for next in 0..stops {
                if mask & (1 << next) != 0 {
                    continue;
                }
                let next_mask = mask | (1 << next);
                let candidate = current + cost[last + 1][next + 1];
                if candidate < best[next_mask][next] {
                    best[next_mask][next] = candidate;
                    parent[next_mask][next] = last;
                }
            }
        }
    }
    let mut last = 0;
    for stop in 1..stops {
        if best[full][stop] < best[full][last] {
            last = stop;
        }
    }
    let mut order = Vec::with_capacity(stops);
    let mut mask = full;
    loop {
        order.push(last + 1);
        let previous = parent[mask][last];
        mask &= !(1 << last);
        if previous == usize::MAX {
            break;
        }
        last = previous;
    }
    order.reverse();
    order
}

fn solve_heuristic(cost: &[Vec<f64>], stops: usize) -> Vec<usize> {
    let mut remaining: Vec<usize> = (1..=stops).collect();
    let mut order = Vec::with_capacity(stops);
    let mut current = 0;
    while !remaining.is_empty() {
        let mut nearest = 0;
        for (index, &stop) in remaining.iter().enumerate() {
            if cost[current][stop] < cost[current][remaining[nearest]] {
                nearest = index;
            }
        }
        current = remaining.remove(nearest);
        order.push(current);
    }
    let mut best = route_cost(cost, &order);
    let mut improved = true;
    while improved {
        improved = false;
        for start in 0..order.len().saturating_sub(1) {
            for end in start + 1..order.len() {
                let mut candidate = order.clone();
                candidate[start..=end].reverse();
                let candidate_cost = route_cost(cost, &candidate);
                if candidate_cost < best {
                    order = candidate;
                    best = candidate_cost;
                    improved = true;
                }
            }
        }
    }
    order
}

/// The stops (1..n) in driving order.
pub fn solve_open_route(cost: &[Vec<f64>]) -> Vec<usize> {
    let stops = cost.len().saturating_sub(1);
    match stops {
        0 => Vec::new(),
        n if n <= EXACT_SOLVE_LIMIT => solve_exact(cost, n),
        n => solve_heuristic(cost, n),
    }
}

/// A timed stop on the route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedStop {
    pub id: i32,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
}

/// Keeps the day's start times and hands them out in route order: the first
/// stop gets the earliest start, and so on. Each event keeps its duration.
pub fn assign_time_slots(stops: &[TimedStop], route: &[i32]) -> Vec<TimedStop> {
    let mut slots: Vec<NaiveDateTime> = stops.iter().map(|stop| stop.start).collect();
    slots.sort_unstable();
    route
        .iter()
        .zip(slots)
        .filter_map(|(id, slot)| {
            let stop = stops.iter().find(|stop| stop.id == *id)?;
            let duration: Duration = stop.end - stop.start;
            Some(TimedStop {
                id: *id,
                start: slot,
                end: slot + duration,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points on a line at these positions; drive time is the distance.
    fn line(positions: &[f64]) -> Vec<Vec<f64>> {
        positions
            .iter()
            .map(|from| positions.iter().map(|to| (from - to).abs()).collect())
            .collect()
    }

    fn brute_force(cost: &[Vec<f64>]) -> f64 {
        fn walk(cost: &[Vec<f64>], order: &mut Vec<usize>, left: &mut Vec<usize>, best: &mut f64) {
            if left.is_empty() {
                *best = best.min(route_cost(cost, order));
                return;
            }
            for index in 0..left.len() {
                let stop = left.remove(index);
                order.push(stop);
                walk(cost, order, left, best);
                order.pop();
                left.insert(index, stop);
            }
        }
        let mut best = f64::INFINITY;
        walk(
            cost,
            &mut Vec::new(),
            &mut (1..cost.len()).collect(),
            &mut best,
        );
        best
    }

    #[test]
    fn visits_stops_outward_along_a_line() {
        assert_eq!(solve_open_route(&line(&[0.0, 30.0, 10.0, 20.0])), [2, 3, 1]);
    }

    #[test]
    fn handles_zero_and_one_stop() {
        assert_eq!(solve_open_route(&line(&[0.0])), Vec::<usize>::new());
        assert_eq!(solve_open_route(&line(&[0.0, 5.0])), [1]);
    }

    #[test]
    fn respects_one_way_times() {
        let cost = vec![
            vec![0.0, 10.0, 10.0],
            vec![10.0, 0.0, 100.0],
            vec![10.0, 1.0, 0.0],
        ];
        assert_eq!(solve_open_route(&cost), [2, 1]);
    }

    #[test]
    fn matches_brute_force_on_small_days() {
        let mut seed: u64 = 7;
        let mut next = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            f64::from(u32::try_from(seed >> 40).unwrap() % 1000)
        };
        for stops in 2..=6 {
            let cost: Vec<Vec<f64>> = (0..=stops)
                .map(|from| {
                    (0..=stops)
                        .map(|to| if from == to { 0.0 } else { next() })
                        .collect()
                })
                .collect();
            let order = solve_open_route(&cost);
            assert!((route_cost(&cost, &order) - brute_force(&cost)).abs() < 1e-9);
        }
    }

    #[test]
    fn large_days_visit_every_stop_once() {
        let positions: Vec<f64> = (0..=16).map(|i| f64::from(i * 7 % 17)).collect();
        let mut order = solve_open_route(&line(&positions));
        order.sort_unstable();
        assert_eq!(order, (1..=16).collect::<Vec<_>>());
    }

    #[test]
    fn start_times_go_out_in_route_order() {
        let at = |h: u32, m: u32| {
            chrono::NaiveDate::from_ymd_opt(2030, 1, 15)
                .unwrap()
                .and_hms_opt(h, m, 0)
                .unwrap()
        };
        let stops = [
            TimedStop {
                id: 1,
                start: at(9, 0),
                end: at(10, 0),
            },
            TimedStop {
                id: 2,
                start: at(11, 0),
                end: at(11, 30),
            },
        ];
        assert_eq!(
            assign_time_slots(&stops, &[2, 1]),
            [
                TimedStop {
                    id: 2,
                    start: at(9, 0),
                    end: at(9, 30)
                },
                TimedStop {
                    id: 1,
                    start: at(11, 0),
                    end: at(12, 0)
                },
            ]
        );
    }
}
