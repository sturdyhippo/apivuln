mod http;
mod tcp;

use std::collections::HashMap;
use std::fmt::Display;

pub use http::*;
pub use tcp::*;

use crate::{Plan, StepBody};

pub struct Executor<'a> {
    plan: &'a Plan<'a>,
    current: Option<usize>,
    outputs: HashMap<&'a str, StepOutput>,
}

impl<'a> Executor<'a> {
    pub fn new(plan: &'a Plan) -> Self {
        Executor {
            plan,
            current: plan.steps.first().map(|_| 0),
            outputs: HashMap::new(),
        }
    }

    pub async fn next(&mut self) -> Result<StepOutput, Box<dyn std::error::Error + Send + Sync>> {
        let Some(current) = &mut self.current else {
            return Err(Box::new(Error::Done));
        };
        let step = &self.plan.steps[*current];
        let inputs = &StepInputs {
            previous: &self.outputs,
        };
        let out = match &step.body {
            StepBody::HTTP(req) => http::execute(&step, &inputs).await?,
            StepBody::TCP(req) => tcp::execute(&step, &inputs).await?,
        };
        let Some(name) = step.name else {
            return Ok(out);
        };
        self.outputs.insert(name, out.clone());
        *current += 1;
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepOutput {
    HTTP(HTTPOutput),
    TCP(TCPOutput),
}

struct StepInputs<'a> {
    previous: &'a HashMap<&'a str, StepOutput>,
}

#[derive(Debug)]
pub enum Error {
    Done,
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("execution done")
    }
}

impl std::error::Error for Error {}
