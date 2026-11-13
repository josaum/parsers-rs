//! Thin PyO3 bindings over [`mcts_fast_core`].
//!
//! All search logic lives in the shared Rust core. This module only marshals
//! Python types ↔ core types and releases the GIL during search.

use mcts_fast_core::{
    mcts_rank as core_mcts_rank, MctsConfig as CoreConfig, MctsEngine as CoreEngine, MctsError,
};
use pyo3::exceptions::PyRuntimeError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

fn map_err(e: MctsError) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

#[pyclass(from_py_object, name = "MctsConfig")]
#[derive(Clone, Debug)]
struct PyMctsConfig {
    #[pyo3(get, set)]
    iterations: u32,
    #[pyo3(get, set)]
    exploration_constant: f64,
    #[pyo3(get, set)]
    max_depth: u32,
    #[pyo3(get, set)]
    gamma: f64,
    #[pyo3(get, set)]
    max_children: u32,
    #[pyo3(get, set)]
    num_parallel: u32,
    #[pyo3(get, set)]
    pw_k: f64,
    #[pyo3(get, set)]
    pw_alpha: f64,
}

#[pymethods]
#[allow(clippy::too_many_arguments)]
impl PyMctsConfig {
    #[new]
    #[pyo3(signature = (
        iterations=200,
        exploration_constant=1.414,
        max_depth=20,
        gamma=0.99,
        max_children=64,
        num_parallel=1,
        pw_k=0.0,
        pw_alpha=0.5,
    ))]
    fn new(
        iterations: u32,
        exploration_constant: f64,
        max_depth: u32,
        gamma: f64,
        max_children: u32,
        num_parallel: u32,
        pw_k: f64,
        pw_alpha: f64,
    ) -> Self {
        Self {
            iterations,
            exploration_constant,
            max_depth,
            gamma,
            max_children,
            num_parallel,
            pw_k,
            pw_alpha,
        }
    }
}

impl From<&PyMctsConfig> for CoreConfig {
    fn from(c: &PyMctsConfig) -> Self {
        CoreConfig {
            iterations: c.iterations,
            exploration_constant: c.exploration_constant,
            max_depth: c.max_depth,
            gamma: c.gamma,
            max_children: c.max_children,
            num_parallel: c.num_parallel.max(1),
            pw_k: c.pw_k,
            pw_alpha: c.pw_alpha,
        }
    }
}

impl From<CoreConfig> for PyMctsConfig {
    fn from(c: CoreConfig) -> Self {
        Self {
            iterations: c.iterations,
            exploration_constant: c.exploration_constant,
            max_depth: c.max_depth,
            gamma: c.gamma,
            max_children: c.max_children,
            num_parallel: c.num_parallel,
            pw_k: c.pw_k,
            pw_alpha: c.pw_alpha,
        }
    }
}

#[pyclass(name = "MctsEngine")]
struct PyMctsEngine {
    inner: CoreEngine,
}

#[pymethods]
impl PyMctsEngine {
    #[new]
    #[pyo3(signature = (config=None))]
    fn new(config: Option<PyMctsConfig>) -> Self {
        let core = config.as_ref().map(CoreConfig::from).unwrap_or_default();
        Self {
            inner: CoreEngine::new(core),
        }
    }

    fn add_root(&mut self, embedding: Vec<f32>, label: String) -> usize {
        self.inner.add_root(embedding, label)
    }

    #[pyo3(signature = (parent_idx, embedding, label, prior=1.0))]
    fn add_child(
        &mut self,
        parent_idx: usize,
        embedding: Vec<f32>,
        label: String,
        prior: f32,
    ) -> PyResult<usize> {
        self.inner
            .add_child(parent_idx, embedding, label, prior)
            .map_err(map_err)
    }

    fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    fn reset(&mut self) {
        self.inner.reset();
    }

    fn search<'py>(
        &mut self,
        py: Python<'py>,
        query_embedding: Vec<f32>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let result = py.detach(|| self.inner.search(&query_embedding).map_err(map_err))?;

        let visit_counts = PyDict::new(py);
        for (label, count) in &result.visit_counts {
            visit_counts.set_item(label, *count)?;
        }

        let out = PyDict::new(py);
        let py_path = PyList::new(py, &result.best_path)?;
        out.set_item(intern!(py, "best_path"), py_path)?;
        out.set_item(intern!(py, "best_reward"), result.best_reward)?;
        out.set_item(intern!(py, "visit_counts"), visit_counts)?;
        out.set_item(intern!(py, "iterations"), result.iterations)?;
        out.set_item(intern!(py, "total_nodes"), result.total_nodes)?;
        Ok(out)
    }

    fn search_json(&mut self, py: Python<'_>, query_embedding: Vec<f32>) -> PyResult<String> {
        py.detach(|| self.inner.search_json(&query_embedding).map_err(map_err))
    }

    fn get_node<'py>(&self, py: Python<'py>, idx: usize) -> PyResult<Bound<'py, PyDict>> {
        let info = self.inner.get_node(idx).map_err(map_err)?;
        let d = PyDict::new(py);
        d.set_item(intern!(py, "label"), info.label)?;
        d.set_item(intern!(py, "visit_count"), info.visit_count)?;
        d.set_item(intern!(py, "q_value"), info.q_value)?;
        d.set_item(intern!(py, "prior"), info.prior)?;
        d.set_item(intern!(py, "children_count"), info.children_count)?;
        d.set_item(intern!(py, "depth"), info.depth)?;
        d.set_item(intern!(py, "virtual_loss"), info.virtual_loss)?;
        Ok(d)
    }
}

#[pyfunction]
#[pyo3(signature = (query_embedding, candidate_embeddings, candidate_labels, iterations=200, exploration_constant=1.414))]
fn mcts_rank(
    py: Python<'_>,
    query_embedding: Vec<f32>,
    candidate_embeddings: Vec<Vec<f32>>,
    candidate_labels: Vec<String>,
    iterations: u32,
    exploration_constant: f64,
) -> PyResult<String> {
    py.detach(move || {
        core_mcts_rank(
            &query_embedding,
            candidate_embeddings,
            candidate_labels,
            iterations,
            exploration_constant,
        )
        .map_err(map_err)
    })
}

#[pymodule]
fn _core(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMctsConfig>()?;
    m.add_class::<PyMctsEngine>()?;
    m.add_function(wrap_pyfunction!(mcts_rank, m)?)?;
    Ok(())
}
