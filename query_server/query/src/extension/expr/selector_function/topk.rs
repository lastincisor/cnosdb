use std::sync::Arc;

use datafusion::{
    arrow::{array::ArrayRef, datatypes::DataType},
    error::DataFusionError,
    logical_expr::{
        aggregate_function::{DATES, NUMERICS, STRINGS, TIMESTAMPS},
        ReturnTypeFunction, ScalarUDF, Signature, TypeSignature, Volatility,
    },
    physical_expr::functions::make_scalar_function,
};

use spi::query::function::{FunctionMetadataManager, Result};

use super::TOPK;

pub fn register_udf(func_manager: &mut dyn FunctionMetadataManager) -> Result<ScalarUDF> {
    let udf = new();
    func_manager.register_udf(udf.clone())?;
    Ok(udf)
}

fn new() -> ScalarUDF {
    let func = |_: &[ArrayRef]| {
        Err(DataFusionError::Execution(format!(
            "{} has no specific implementation, should be converted to topk operator.",
            TOPK
        )))
    };
    let func = make_scalar_function(func);

    // Accept any numeric value paired with a float64 percentile
    let type_signatures = STRINGS
        .iter()
        .chain(NUMERICS.iter())
        .chain(TIMESTAMPS.iter())
        .chain(DATES.iter())
        // .chain(TIMES.iter())
        .map(|t| TypeSignature::Exact(vec![t.clone(), DataType::Int64]))
        .collect();

    let signature = Signature::one_of(type_signatures, Volatility::Immutable);

    let return_type: ReturnTypeFunction =
        Arc::new(move |input_expr_types| Ok(Arc::new(input_expr_types[0].clone())));

    ScalarUDF::new(TOPK, &signature, &return_type, &func)
}