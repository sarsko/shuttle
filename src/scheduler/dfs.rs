use crate::runtime::task::TaskId;
use crate::scheduler::data::fixed::FixedDataSource;
use crate::scheduler::data::DataSource;
use crate::scheduler::{Schedule, Scheduler};

const DFS_RANDOM_SEED: u64 = 0x12345678;

/// A scheduler that performs an exhaustive, depth-first enumeration of all possible schedules.
#[derive(Debug)]
pub struct DfsScheduler {
    max_iterations: Option<usize>,
    allow_random_data: bool,

    iterations: usize,
    // Vec<(previous choice, was that the last choice at that level)>
    levels: Vec<(TaskId, bool)>,
    steps: usize,

    data_source: FixedDataSource,
}

impl DfsScheduler {
    /// Construct a new DFSScheduler with an optional bound on how many iterations to run. A
    /// DFSScheduler can optionally allow random data to be generated by the test (using
    /// `shuttle::rand`). Enabling random data makes the DFS search incomplete, as it will not
    /// explore all possible values for the random choices. To ensure determinism, each execution
    /// will use the same sequence of random choices.
    pub fn new(max_iterations: Option<usize>, allow_random_data: bool) -> Self {
        let data_source = FixedDataSource::initialize(DFS_RANDOM_SEED);

        Self {
            max_iterations,
            iterations: 0,
            levels: vec![],
            steps: 0,
            allow_random_data,
            data_source,
        }
    }

    /// Check if there are any scheduling points at or below the `index`th level that have remaining
    /// schedulable tasks to explore.
    // TODO probably should memoize this -- at each iteration, just need to know the largest i
    // TODO such that levels[i].1 == true, which is the place we'll make a change
    fn has_more_choices(&self, index: usize) -> bool {
        self.levels[index..].iter().any(|(_, last)| !*last)
    }
}

impl Scheduler for DfsScheduler {
    fn new_execution(&mut self) -> Option<Schedule> {
        if self.max_iterations.map(|mi| self.iterations >= mi).unwrap_or(false) {
            return None;
        }

        // If there are no more choices to make at any level, we're done
        if self.iterations > 0 && !self.has_more_choices(0) {
            return None;
        }

        self.iterations += 1;
        self.steps = 0;

        Some(Schedule::new(self.data_source.reinitialize()))
    }

    fn next_task(&mut self, runnable: &[TaskId], _current: Option<TaskId>) -> Option<TaskId> {
        let next = if self.steps >= self.levels.len() {
            // First time we've reached this level
            assert_eq!(self.steps, self.levels.len());
            let to_run = runnable.first().unwrap();
            self.levels.push((*to_run, runnable.len() == 1));
            *to_run
        } else {
            let (last_choice, was_last) = self.levels[self.steps];
            if self.has_more_choices(self.steps + 1) {
                // Keep the same choice, because there's more work to do somewhere below us
                last_choice
            } else {
                // Time to make a change at this level
                assert!(
                    !was_last,
                    "if we are making a change, there should be another available option"
                );
                let next_idx = runnable.iter().position(|tid| *tid == last_choice).unwrap() + 1;
                let next = runnable[next_idx];
                self.levels.drain(self.steps..);
                self.levels.push((next, next_idx == runnable.len() - 1));
                next
            }
        };

        self.steps += 1;

        Some(next)
    }

    fn next_u64(&mut self) -> u64 {
        if !self.allow_random_data {
            panic!("requested random data from DFS scheduler with allow_random_data = false");
        }
        self.data_source.next_u64()
    }
}
