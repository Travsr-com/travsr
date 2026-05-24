use pyo3::prelude::*;

#[pyfunction]
pub fn multiply(a: i64, b: i64) -> i64 {
    a * b
}

#[pyfunction]
#[pyo3(name = "upper")]
pub fn to_upper(s: String) -> String {
    s.to_uppercase()
}

#[pymodule]
fn _native(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(multiply, m)?)?;
    m.add_function(wrap_pyfunction!(to_upper, m)?)?;
    Ok(())
}
