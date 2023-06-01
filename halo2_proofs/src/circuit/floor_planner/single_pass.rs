use std::cmp;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ff::Field;

use ark_std::{end_timer, start_timer};

use crate::{
    circuit::{
        layouter::{RegionColumn, RegionLayouter, RegionShape, TableLayouter},
        Cell, Layouter, Region, RegionIndex, RegionStart, Table, Value,
    },
    multicore,
    plonk::{
        Advice, Any, Assigned, Assignment, Challenge, Circuit, Column, Error, Fixed, FloorPlanner,
        Instance, Selector, TableColumn,
    },
};

/// A simple [`FloorPlanner`] that performs minimal optimizations.
///
/// This floor planner is suitable for debugging circuits. It aims to reflect the circuit
/// "business logic" in the circuit layout as closely as possible. It uses a single-pass
/// layouter that does not reorder regions for optimal packing.
#[derive(Debug)]
pub struct SimpleFloorPlanner;

/// SimpleFloorPlanner是对FloorPlanner接口的实现
impl FloorPlanner for SimpleFloorPlanner {
    /// SimpleFloorPlanner实现了FloorPlanner接口的synthesize函数。
    /// 该函数创建出SingleChipLayouter对象，并直接调用相应的synthesize函数开始电路的synthesize。
    fn synthesize<F: Field, CS: Assignment<F>, C: Circuit<F>>(
        cs: &mut CS,
        circuit: &C,
        config: C::Config,
        constants: Vec<Column<Fixed>>,
    ) -> Result<(), Error> {
        let timer = start_timer!(|| ("SimpleFloorPlanner synthesize").to_string());
        let layouter = SingleChipLayouter::new(cs, constants)?;
        let result = circuit.synthesize(config, layouter);
        end_timer!(timer);
        result
    }
}

/// A [`Layouter`] for a single-chip circuit.
pub struct SingleChipLayouter<'a, F: Field, CS: Assignment<F> + 'a> {
    cs: &'a mut CS,
    constants: Vec<Column<Fixed>>,
    /// Stores the starting row for each region.
    regions: Vec<RegionStart>,
    /// Stores the first empty row for each column.
    columns: HashMap<RegionColumn, usize>,
    /// Stores the table fixed columns.
    table_columns: Vec<TableColumn>,
    _marker: PhantomData<F>,
}

impl<'a, F: Field, CS: Assignment<F> + 'a> fmt::Debug for SingleChipLayouter<'a, F, CS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SingleChipLayouter")
            .field("regions", &self.regions)
            .field("columns", &self.columns)
            .finish()
    }
}

impl<'a, F: Field, CS: Assignment<F> + 'a> SingleChipLayouter<'a, F, CS> {
    /// Creates a new single-chip layouter.
    pub fn new(cs: &'a mut CS, constants: Vec<Column<Fixed>>) -> Result<Self, Error> {
        let ret = SingleChipLayouter {
            cs,
            constants,
            regions: vec![],
            columns: HashMap::default(),
            table_columns: vec![],
            _marker: PhantomData,
        };
        Ok(ret)
    }

    fn fork(&self, sub_cs: Vec<&'a mut CS>) -> Result<Vec<Self>, Error> {
        Ok(sub_cs
            .into_iter()
            .map(|sub_cs| Self {
                cs: sub_cs,
                constants: self.constants.clone(),
                regions: self.regions.clone(),
                columns: self.columns.clone(),
                table_columns: self.table_columns.clone(),
                _marker: Default::default(),
            })
            .collect::<Vec<_>>())
    }
}

impl<'a, F: Field, CS: Assignment<F> + 'a> Layouter<F> for SingleChipLayouter<'a, F, CS> {
    type Root = Self;

    fn assign_region<A, AR, N, NR>(&mut self, name: N, mut assignment: A) -> Result<AR, Error>
    where
        A: FnMut(Region<'_, F>) -> Result<AR, Error>,
        N: Fn() -> NR,
        NR: Into<String>,
    {
        let region_name: String = name().into();
        let timer = start_timer!(|| format!("assign region: {}", region_name));
        let region_index = self.regions.len(); // 获取当前region位置

        // Get shape of the region.
        // 调用RegionShape，收集该Region的电路涉及到的Column信息
        let mut shape = RegionShape::new(region_index.into());
        {
            let timer_1st = start_timer!(|| format!("assign region 1st pass: {}", region_name));
            let region: &mut dyn RegionLayouter<F> = &mut shape;
            assignment(region.into())?;
            end_timer!(timer_1st);
        }
        let row_count = shape.row_count();
        let log_region_info = row_count >= 40;
        if log_region_info {
            log::debug!(
                "region row_count \"{}\": {}",
                region_name,
                shape.row_count()
            );
        }

        // Lay out this region. We implement the simplest approach here: position the
        // region starting at the earliest row for which none of the columns are in use.
        // 布置这个区域。我们在这里实施最简单的方法：将区域定位在没有使用任何列的最早行。
        // 根据收集到的Column信息，获取Region开始的行号
        let mut region_start = 0;
        for column in &shape.columns {
            let column_start = self.columns.get(column).cloned().unwrap_or(0);
            if column_start != 0 && log_region_info {
                log::trace!(
                    "columns {:?} reused between multi regions. Start: {}. Region: \"{}\"",
                    column,
                    column_start,
                    region_name
                );
            }
            region_start = cmp::max(region_start, column_start);
        }
        if log_region_info {
            log::debug!(
                "region \"{}\", idx {} start {}",
                region_name,
                self.regions.len(),
                region_start
            );
        }
        self.regions.push(region_start.into());

        // Update column usage information.
        // 在Region中记录所有使用的Column信息
        for column in shape.columns {
            self.columns.insert(column, region_start + shape.row_count);
        }

        // Assign region cells.
        // 分配region cell
        // 创建Region
        self.cs.enter_region(name);
        let mut region = SingleChipLayouterRegion::new(self, region_index.into());
        let result = {
            let timer_2nd = start_timer!(|| format!("assign region 2nd pass: {}", region_name));
            let region: &mut dyn RegionLayouter<F> = &mut region;
            // 采用SingleChipLayouterRegion对电路赋值
            let result = assignment(region.into());
            end_timer!(timer_2nd);
            result
        }?;
        let constants_to_assign = region.constants;
        // 退出Region
        self.cs.exit_region();

        // Assign constants. For the simple floor planner, we assign constants in order in
        // the first `constants` column.
        // 如果制定了constants，需要增加置换限制
        if self.constants.is_empty() {
            if !constants_to_assign.is_empty() {
                return Err(Error::NotEnoughColumnsForConstants);
            }
        } else {
            let constants_column = self.constants[0];
            let next_constant_row = self
                .columns
                .entry(Column::<Any>::from(constants_column).into())
                .or_default();
            for (constant, advice) in constants_to_assign {
                self.cs.assign_fixed(
                    || format!("Constant({:?})", constant.evaluate()),
                    constants_column,
                    *next_constant_row,
                    || Value::known(constant),
                )?;
                self.cs.copy(
                    constants_column.into(),
                    *next_constant_row,
                    advice.column,
                    *self.regions[*advice.region_index] + advice.row_offset,
                )?;
                *next_constant_row += 1;
            }
        }

        end_timer!(timer);
        Ok(result)
    }

    #[cfg(feature = "parallel_syn")]
    fn assign_regions<A, AR, N, NR>(
        &mut self,
        name: N,
        mut assignments: Vec<A>,
    ) -> Result<Vec<AR>, Error>
    where
        A: FnMut(Region<'_, F>) -> Result<AR, Error> + Send,
        AR: Send,
        N: Fn() -> NR,
        NR: Into<String>,
    {
        let region_index = self.regions.len();
        let region_name: String = name().into();
        // Get region shapes sequentially
        let mut ranges = vec![];
        for (i, assignment) in assignments.iter_mut().enumerate() {
            // Get shape of the ith sub-region.
            let mut shape = RegionShape::new((region_index + i).into());
            let region: &mut dyn RegionLayouter<F> = &mut shape;
            assignment(region.into())?;

            let mut region_start = 0;
            for column in &shape.columns {
                let column_start = self.columns.get(column).cloned().unwrap_or(0);
                region_start = cmp::max(region_start, column_start);
            }
            log::debug!(
                "{}_{} start: {}, end: {}",
                region_name,
                i,
                region_start,
                region_start + shape.row_count()
            );
            self.regions.push(region_start.into());
            ranges.push(region_start..(region_start + shape.row_count()));

            // Update column usage information.
            for column in shape.columns.iter() {
                self.columns
                    .insert(*column, region_start + shape.row_count());
            }
        }

        // Do actual synthesis of sub-regions in parallel
        let cs_fork_time = Instant::now();
        let mut sub_cs = self.cs.fork(&ranges)?;
        log::info!(
            "CS forked into {} subCS took {:?}",
            sub_cs.len(),
            cs_fork_time.elapsed()
        );
        let ref_sub_cs = sub_cs.iter_mut().collect();
        let sub_layouters = self.fork(ref_sub_cs)?;
        let regions_2nd_pass = Instant::now();
        let ret = crossbeam::scope(|scope| {
            let mut handles = vec![];
            for (i, (mut assignment, mut sub_layouter)) in assignments
                .into_iter()
                .zip(sub_layouters.into_iter())
                .enumerate()
            {
                let region_name = format!("{}_{}", region_name, i);
                handles.push(scope.spawn(move |_| {
                    let sub_region_2nd_pass = Instant::now();
                    sub_layouter.cs.enter_region(|| region_name.clone());
                    let mut region =
                        SingleChipLayouterRegion::new(&mut sub_layouter, (region_index + i).into());
                    let region_ref: &mut dyn RegionLayouter<F> = &mut region;
                    let result = assignment(region_ref.into());
                    let constant = region.constants.clone();
                    sub_layouter.cs.exit_region();
                    log::info!(
                        "region {} 2nd pass synthesis took {:?}",
                        region_name,
                        sub_region_2nd_pass.elapsed()
                    );

                    (result, constant)
                }));
            }

            handles
                .into_iter()
                .map(|handle| handle.join().expect("handle.join should never fail"))
                .collect::<Vec<_>>()
        })
        .expect("scope should not fail");
        let cs_merge_time = Instant::now();
        let num_sub_cs = sub_cs.len();
        self.cs.merge(sub_cs)?;
        log::info!(
            "Merge {} subCS back took {:?}",
            num_sub_cs,
            cs_merge_time.elapsed()
        );
        log::info!(
            "{} sub_regions of {} 2nd pass synthesis took {:?}",
            ranges.len(),
            region_name,
            regions_2nd_pass.elapsed()
        );
        let (results, constants): (Vec<_>, Vec<_>) = ret.into_iter().unzip();

        // Check if there are errors in sub-region synthesis
        let results = results.into_iter().collect::<Result<Vec<_>, Error>>()?;

        // Merge all constants from sub-regions together
        let constants_to_assign = constants
            .into_iter()
            .flat_map(|constant_to_assign| constant_to_assign.into_iter())
            .collect::<Vec<_>>();

        // Assign constants. For the simple floor planner, we assign constants in order in
        // the first `constants` column.
        if self.constants.is_empty() {
            if !constants_to_assign.is_empty() {
                return Err(Error::NotEnoughColumnsForConstants);
            }
        } else {
            let constants_column = self.constants[0];
            let next_constant_row = self
                .columns
                .entry(Column::<Any>::from(constants_column).into())
                .or_default();
            for (constant, advice) in constants_to_assign {
                self.cs.assign_fixed(
                    || format!("Constant({:?})", constant.evaluate()),
                    constants_column,
                    *next_constant_row,
                    || Value::known(constant),
                )?;
                self.cs.copy(
                    constants_column.into(),
                    *next_constant_row,
                    advice.column,
                    *self.regions[*advice.region_index] + advice.row_offset,
                )?;
                *next_constant_row += 1;
            }
        }

        Ok(results)
    }

    fn assign_table<A, N, NR>(&mut self, name: N, mut assignment: A) -> Result<(), Error>
    where
        A: FnMut(Table<'_, F>) -> Result<(), Error>,
        N: Fn() -> NR,
        NR: Into<String>,
    {
        // Maintenance hazard: there is near-duplicate code in `v1::AssignmentPass::assign_table`.
        // Assign table cells.
        // 创建Region
        self.cs.enter_region(name);
        let mut table = SimpleTableLayouter::new(self.cs, &self.table_columns);
        {
            let table: &mut dyn TableLayouter<F> = &mut table;
            // 查找表赋值
            assignment(table.into())
        }?;
        let default_and_assigned = table.default_and_assigned;
        // 退出Region
        self.cs.exit_region();

        // Check that all table columns have the same length `first_unused`,
        // and all cells up to that length are assigned.
        // 检查所有表格列是否具有相同的长度“first_unused”，并且分配了不超过该长度的所有单元格。
        // 获取出所有查找表相关的Column对应的最大使用行数
        let first_unused = {
            match default_and_assigned
                .values()
                .map(|(_, assigned)| {
                    if assigned.iter().all(|b| *b) {
                        Some(assigned.len())
                    } else {
                        None
                    }
                })
                .reduce(|acc, item| match (acc, item) {
                    (Some(a), Some(b)) if a == b => Some(a),
                    _ => None,
                }) {
                Some(Some(len)) => len,
                _ => return Err(Error::Synthesis), // TODO better error
            }
        };

        // default_and_assigned记录了在一个Fixed Column上的default值以及某个cell是否已经设置
        // Record these columns so that we can prevent them from being used again.
        for column in default_and_assigned.keys() {
            self.table_columns.push(*column);
        }

        // 根据default_and_assigned信息，采用default值扩展所有的column
        for (col, (default_val, _)) in default_and_assigned {
            // default_val must be Some because we must have assigned
            // at least one cell in each column, and in that case we checked
            // that all cells up to first_unused were assigned.
            // default_val必须是Some，因为我们必须在每一列中至少分配一个单元格，
            // 在这种情况下，我们检查是否分配了直到first_unused的所有单元格
            self.cs
                .fill_from_row(col.inner(), first_unused, default_val.unwrap())?;
        }

        Ok(())
    }

    fn constrain_instance(
        &mut self,
        cell: Cell,
        instance: Column<Instance>,
        row: usize,
    ) -> Result<(), Error> {
        self.cs.copy(
            cell.column,
            *self.regions[*cell.region_index] + cell.row_offset,
            instance.into(),
            row,
        )
    }

    fn get_challenge(&self, challenge: Challenge) -> Value<F> {
        self.cs.get_challenge(challenge)
    }

    fn get_root(&mut self) -> &mut Self::Root {
        self
    }

    fn push_namespace<NR, N>(&mut self, name_fn: N)
    where
        NR: Into<String>,
        N: FnOnce() -> NR,
    {
        self.cs.push_namespace(name_fn)
    }

    fn pop_namespace(&mut self, gadget_name: Option<String>) {
        self.cs.pop_namespace(gadget_name)
    }
}

struct SingleChipLayouterRegion<'r, 'a, F: Field, CS: Assignment<F> + 'a> {
    layouter: &'r mut SingleChipLayouter<'a, F, CS>,
    region_index: RegionIndex,
    /// Stores the constants to be assigned, and the cells to which they are copied.
    constants: Vec<(Assigned<F>, Cell)>,
}

impl<'r, 'a, F: Field, CS: Assignment<F> + 'a> fmt::Debug
    for SingleChipLayouterRegion<'r, 'a, F, CS>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SingleChipLayouterRegion")
            .field("layouter", &self.layouter)
            .field("region_index", &self.region_index)
            .finish()
    }
}

impl<'r, 'a, F: Field, CS: Assignment<F> + 'a> SingleChipLayouterRegion<'r, 'a, F, CS> {
    fn new(layouter: &'r mut SingleChipLayouter<'a, F, CS>, region_index: RegionIndex) -> Self {
        SingleChipLayouterRegion {
            layouter,
            region_index,
            constants: vec![],
        }
    }
}

impl<'r, 'a, F: Field, CS: Assignment<F> + 'a> RegionLayouter<F>
    for SingleChipLayouterRegion<'r, 'a, F, CS>
{
    fn enable_selector<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        selector: &Selector,
        offset: usize,
    ) -> Result<(), Error> {
        self.layouter.cs.enable_selector(
            annotation,
            selector,
            *self.layouter.regions[*self.region_index] + offset,
        )
    }

    fn name_column<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        column: Column<Any>,
    ) {
        self.layouter.cs.annotate_column(annotation, column);
    }

    /// 给一个Advice列中Cell赋值
    fn assign_advice<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        column: Column<Advice>,
        offset: usize,
        to: &'v mut (dyn FnMut() -> Value<Assigned<F>> + 'v),
    ) -> Result<Cell, Error> {
        // 调用Assignment接口设置相应的Cell信息，特别注意的是在设置的时候需要Cell的全局偏移
        self.layouter.cs.assign_advice(
            annotation,
            column,
            *self.layouter.regions[*self.region_index] + offset,
            to,
        )?;

        Ok(Cell {
            region_index: self.region_index,
            row_offset: offset,
            column: column.into(),
        })
    }

    fn assign_advice_from_constant<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        column: Column<Advice>,
        offset: usize,
        constant: Assigned<F>,
    ) -> Result<Cell, Error> {
        let advice =
            self.assign_advice(annotation, column, offset, &mut || Value::known(constant))?;
        self.constrain_constant(advice, constant)?;

        Ok(advice)
    }

    fn assign_advice_from_instance<'v>(
        &mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        instance: Column<Instance>,
        row: usize,
        advice: Column<Advice>,
        offset: usize,
    ) -> Result<(Cell, Value<F>), Error> {
        let value = self.layouter.cs.query_instance(instance, row)?;

        let cell = self.assign_advice(annotation, advice, offset, &mut || value.to_field())?;

        self.layouter.cs.copy(
            cell.column,
            *self.layouter.regions[*cell.region_index] + cell.row_offset,
            instance.into(),
            row,
        )?;

        Ok((cell, value))
    }

    fn assign_fixed<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        column: Column<Fixed>,
        offset: usize,
        to: &'v mut (dyn FnMut() -> Value<Assigned<F>> + 'v),
    ) -> Result<Cell, Error> {
        self.layouter.cs.assign_fixed(
            annotation,
            column,
            *self.layouter.regions[*self.region_index] + offset,
            to,
        )?;

        Ok(Cell {
            region_index: self.region_index,
            row_offset: offset,
            column: column.into(),
        })
    }

    fn constrain_constant(&mut self, cell: Cell, constant: Assigned<F>) -> Result<(), Error> {
        self.constants.push((constant, cell));
        Ok(())
    }

    fn constrain_equal(&mut self, left: Cell, right: Cell) -> Result<(), Error> {
        self.layouter.cs.copy(
            left.column,
            *self.layouter.regions[*left.region_index] + left.row_offset,
            right.column,
            *self.layouter.regions[*right.region_index] + right.row_offset,
        )?;

        Ok(())
    }
}

/// The default value to fill a table column with.
///
/// - The outer `Option` tracks whether the value in row 0 of the table column has been
///   assigned yet. This will always be `Some` once a valid table has been completely
///   assigned.
/// - The inner `Value` tracks whether the underlying `Assignment` is evaluating
///   witnesses or not.
type DefaultTableValue<F> = Option<Value<Assigned<F>>>;

pub(crate) struct SimpleTableLayouter<'r, 'a, F: Field, CS: Assignment<F> + 'a> {
    cs: &'a mut CS,
    used_columns: &'r [TableColumn],
    // maps from a fixed column to a pair (default value, vector saying which rows are assigned)
    pub(crate) default_and_assigned: HashMap<TableColumn, (DefaultTableValue<F>, Vec<bool>)>,
}

impl<'r, 'a, F: Field, CS: Assignment<F> + 'a> fmt::Debug for SimpleTableLayouter<'r, 'a, F, CS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimpleTableLayouter")
            .field("used_columns", &self.used_columns)
            .field("default_and_assigned", &self.default_and_assigned)
            .finish()
    }
}

impl<'r, 'a, F: Field, CS: Assignment<F> + 'a> SimpleTableLayouter<'r, 'a, F, CS> {
    pub(crate) fn new(cs: &'a mut CS, used_columns: &'r [TableColumn]) -> Self {
        SimpleTableLayouter {
            cs,
            used_columns,
            default_and_assigned: HashMap::default(),
        }
    }
}

impl<'r, 'a, F: Field, CS: Assignment<F> + 'a> TableLayouter<F>
    for SimpleTableLayouter<'r, 'a, F, CS>
{
    fn assign_cell<'v>(
        &'v mut self,
        annotation: &'v (dyn Fn() -> String + 'v),
        column: TableColumn,
        offset: usize,
        to: &'v mut (dyn FnMut() -> Value<Assigned<F>> + 'v),
    ) -> Result<(), Error> {
        if self.used_columns.contains(&column) {
            return Err(Error::Synthesis); // TODO better error
        }

        let entry = self.default_and_assigned.entry(column).or_default();

        let mut value = Value::unknown();
        self.cs.assign_fixed(
            annotation,
            column.inner(),
            offset, // tables are always assigned starting at row 0
            || {
                let res = to();
                value = res;
                res
            },
        )?;

        match (entry.0.is_none(), offset) {
            // Use the value at offset 0 as the default value for this table column.
            (true, 0) => entry.0 = Some(value),
            // Since there is already an existing default value for this table column,
            // the caller should not be attempting to assign another value at offset 0.
            (false, 0) => return Err(Error::Synthesis), // TODO better error
            _ => (),
        }
        if entry.1.len() <= offset {
            entry.1.resize(offset + 1, false);
        }
        entry.1[offset] = true;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use halo2curves::pasta::vesta;

    use super::SimpleFloorPlanner;
    use crate::{
        dev::MockProver,
        plonk::{Advice, Circuit, Column, Error},
    };

    #[test]
    fn not_enough_columns_for_constants() {
        struct MyCircuit {}

        impl Circuit<vesta::Scalar> for MyCircuit {
            type Config = Column<Advice>;
            type FloorPlanner = SimpleFloorPlanner;

            fn without_witnesses(&self) -> Self {
                MyCircuit {}
            }

            fn configure(meta: &mut crate::plonk::ConstraintSystem<vesta::Scalar>) -> Self::Config {
                meta.advice_column()
            }

            fn synthesize(
                &self,
                config: Self::Config,
                mut layouter: impl crate::circuit::Layouter<vesta::Scalar>,
            ) -> Result<(), crate::plonk::Error> {
                layouter.assign_region(
                    || "assign constant",
                    |mut region| {
                        region.assign_advice_from_constant(
                            || "one",
                            config,
                            0,
                            vesta::Scalar::one(),
                        )
                    },
                )?;

                Ok(())
            }
        }

        let circuit = MyCircuit {};
        assert!(matches!(
            MockProver::run(3, &circuit, vec![]).unwrap_err(),
            Error::NotEnoughColumnsForConstants,
        ));
    }
}
