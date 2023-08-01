//! Chip that implements instructions to check: a * b + c == d (mod 2^256) where
//! a, b, c and d are all 256-bit words.
//!
//! The circuit layout is as follows:
#[rustfmt::skip]
// | q_step | col0      | col1      | col2      | col3      | col4      |
// |--------|-----------|-----------|-----------|-----------|-----------|
// | 1      | a_limb0   | a_limb1   | a_limb2   | a_limb3   | -         |
// | 0      | b_limb0   | b_limb1   | b_limb2   | b_limb3   | -         |
// | 0      | c_lo      | c_hi      | d_lo      | d_hi      | -         |
// | 0      | carry_lo0 | carry_lo1 | carry_lo2 | carry_lo3 | carry_lo4 |
// | 0      | carry_lo5 | carry_lo6 | carry_lo7 | carry_lo8 | -         |
// | 0      | carry_hi0 | carry_hi1 | carry_hi2 | carry_hi3 | carry_hi4 |
// | 0      | carry_hi5 | carry_hi6 | carry_hi7 | carry_hi8 | -         |
// |--------|-----------|-----------|-----------|-----------|-----------|

use eth_types::{Field, ToLittleEndian, Word};

use halo2_proofs::{
    circuit::{Region, Value},
    plonk::{Advice, Column, ConstraintSystem, Error, Expression, TableColumn, VirtualCells},
    poly::Rotation,
};

use crate::{
    less_than::{LtChip, LtInstruction},
    util::{expr_from_bytes, or, pow_of_two, split_u256, split_u256_limb64, Expr},
};

/// Config for the MulAddChip.
#[derive(Clone, Debug)]
pub struct MulAddConfig<F> {
    /// First of the columns which we use over multiple rows to represent the
    /// schema described above.
    pub col0: Column<Advice>,
    /// Second of the columns which we use over multiple rows to represent the
    /// schema described above.
    pub col1: Column<Advice>,
    /// Third of the columns which we use over multiple rows to represent the
    /// schema described above.
    pub col2: Column<Advice>,
    /// Fourth of the columns which we use over multiple rows to represent the
    /// schema described above.
    pub col3: Column<Advice>,
    /// Fifth of the columns which we use over multiple rows to represent the
    /// schema described above.
    pub col4: Column<Advice>,
    /// Sum of the parts higher than 256-bit in the product.
    pub overflow: Expression<F>,
    /// Range check of carry_lo, carry_hi which needs to be in [0, 2^8)
    /// also, Lookup table for LtChips
    pub u16_table: TableColumn,
    /// Range check of a, b which needs to be in [0, 2^64)
    pub range_check_64: [(LtChip<F, 8>, LtChip<F, 8>); 4],
    /// Range check of c, d which needs to be in [0, 2^128)
    pub range_check_128: [(LtChip<F, 16>, LtChip<F, 16>); 2],
}

impl<F: Field> MulAddConfig<F> {
    /// 64-bit limbs representing `a` from the equation `a * b + c == d`.
    pub fn a_limbs_cur(
        &self,
        meta: &mut VirtualCells<'_, F>,
    ) -> (Expression<F>, Expression<F>, Expression<F>, Expression<F>) {
        (
            meta.query_advice(self.col0, Rotation::cur()),
            meta.query_advice(self.col1, Rotation::cur()),
            meta.query_advice(self.col2, Rotation::cur()),
            meta.query_advice(self.col3, Rotation::cur()),
        )
    }

    /// 64-bit limbs representing `b` from the equation `a * b + c == d`.
    pub fn b_limbs_cur(
        &self,
        meta: &mut VirtualCells<'_, F>,
    ) -> (Expression<F>, Expression<F>, Expression<F>, Expression<F>) {
        (
            meta.query_advice(self.col0, Rotation(1)),
            meta.query_advice(self.col1, Rotation(1)),
            meta.query_advice(self.col2, Rotation(1)),
            meta.query_advice(self.col3, Rotation(1)),
        )
    }

    /// 128-bit lo-hi parts of `c` from the equation `a * b + c == d`.
    pub fn c_lo_hi_cur(&self, meta: &mut VirtualCells<'_, F>) -> (Expression<F>, Expression<F>) {
        (
            meta.query_advice(self.col0, Rotation(2)),
            meta.query_advice(self.col1, Rotation(2)),
        )
    }

    /// 128-bit lo-hi parts of `d` from the equation `a * b + c == d`.
    pub fn d_lo_hi_cur(&self, meta: &mut VirtualCells<'_, F>) -> (Expression<F>, Expression<F>) {
        (
            meta.query_advice(self.col2, Rotation(2)),
            meta.query_advice(self.col3, Rotation(2)),
        )
    }

    /// 128-bit lo-hi parts of `d` from the next step.
    pub fn d_lo_hi_next(&self, meta: &mut VirtualCells<'_, F>) -> (Expression<F>, Expression<F>) {
        (
            meta.query_advice(self.col2, Rotation(9)),
            meta.query_advice(self.col3, Rotation(9)),
        )
    }
}

/// Chip to constrain a * b + c == d (mod 2^256).
#[derive(Clone, Debug)]
pub struct MulAddChip<F> {
    /// Config for the chip.
    pub config: MulAddConfig<F>,
}

impl<F: Field> MulAddChip<F> {
    /// Configure the MulAdd chip.
    #[allow(clippy::too_many_arguments)]
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        q_enable: impl FnOnce(&mut VirtualCells<'_, F>) -> Expression<F> + Clone,
        u16_table: TableColumn,
    ) -> MulAddConfig<F> {
        let col0 = meta.advice_column();
        let col1 = meta.advice_column();
        let col2 = meta.advice_column();
        let col3 = meta.advice_column();
        let col4 = meta.advice_column();
        let mut overflow = 0.expr();

        let carry_lo_cols = [col0, col1, col2, col3, col4]
            .map(|col| (col, 3))
            .into_iter()
            .chain([col0, col1, col2, col3].map(|col| (col, 4)))
            .collect::<Vec<_>>();

        let carry_hi_cols = [col0, col1, col2, col3, col4]
            .map(|col| (col, 5))
            .into_iter()
            .chain([col0, col1, col2, col3].map(|col| (col, 6)))
            .collect::<Vec<_>>();

        let carry_lo_expr = |meta: &mut VirtualCells<F>| {
            carry_lo_cols
                .iter()
                .map(|(col, rot)| meta.query_advice(*col, Rotation(*rot)))
                .collect::<Vec<Expression<F>>>()
        };

        let carry_hi_expr = |meta: &mut VirtualCells<F>| {
            carry_hi_cols
                .iter()
                .map(|(col, rot)| meta.query_advice(*col, Rotation(*rot)))
                .collect::<Vec<Expression<F>>>()
        };

        let range_check_64_config = [col0, col1, col2, col3].map(|col| {
            let config_a = LtChip::configure(
                meta,
                q_enable.clone(),
                |_| Expression::Constant(F::from(u64::MAX)),
                |meta| meta.query_advice(col, Rotation(0)),
                u16_table,
            );
            let config_b = LtChip::configure(
                meta,
                q_enable.clone(),
                |_| Expression::Constant(F::from(u64::MAX)),
                |meta| meta.query_advice(col, Rotation(1)),
                u16_table,
            );

            (config_a, config_b)
        });

        let range_check_128_config = [(col0, col2), (col1, col3)].map(|(c, d)| {
            let config_c = LtChip::configure(
                meta,
                q_enable.clone(),
                |_| Expression::Constant(F::from_u128(u128::MAX)),
                |meta| meta.query_advice(c, Rotation(2)),
                u16_table,
            );
            let config_d = LtChip::configure(
                meta,
                q_enable.clone(),
                |_| Expression::Constant(F::from_u128(u128::MAX)),
                |meta| meta.query_advice(d, Rotation(2)),
                u16_table,
            );
            (config_c, config_d)
        });

        {
            let mut carry_cols = carry_lo_cols.clone();
            carry_cols.append(&mut carry_hi_cols.clone());

            for cols in carry_cols.chunks(2) {
                debug_assert_eq!(cols.len(), 2);
                meta.lookup("mul carry range check lo/hi lookup u8", |meta| {
                    let q_enable = q_enable.clone()(meta);
                    let cell1 = meta.query_advice(cols[0].0, Rotation(cols[0].1));
                    let cell2 = meta.query_advice(cols[1].0, Rotation(cols[1].1));
                    let expr = cell1 * Expression::Constant(pow_of_two(8)) + cell2;
                    vec![(q_enable * expr, u16_table)]
                });
            }
        }

        meta.create_gate("mul add gate", |meta| {
            let q_enable = q_enable(meta);

            let a_limbs =
                [col0, col1, col2, col3].map(|column| meta.query_advice(column, Rotation::cur()));
            let b_limbs =
                [col0, col1, col2, col3].map(|column| meta.query_advice(column, Rotation::next()));

            let c_lo = meta.query_advice(col0, Rotation(2));
            let c_hi = meta.query_advice(col1, Rotation(2));
            let d_lo = meta.query_advice(col2, Rotation(2));
            let d_hi = meta.query_advice(col3, Rotation(2));

            let carry_los = carry_lo_expr(meta);
            let carry_his = carry_hi_expr(meta);

            let carry_lo_expr = expr_from_bytes(&carry_los);
            let carry_hi_expr = expr_from_bytes(&carry_his);

            let t0 = a_limbs[0].clone() * b_limbs[0].clone();
            let t1 =
                a_limbs[0].clone() * b_limbs[1].clone() + a_limbs[1].clone() * b_limbs[0].clone();
            let t2 = a_limbs[0].clone() * b_limbs[2].clone()
                + a_limbs[1].clone() * b_limbs[1].clone()
                + a_limbs[2].clone() * b_limbs[0].clone();
            let t3 = a_limbs[0].clone() * b_limbs[3].clone()
                + a_limbs[1].clone() * b_limbs[2].clone()
                + a_limbs[2].clone() * b_limbs[1].clone()
                + a_limbs[3].clone() * b_limbs[0].clone();
            overflow = carry_hi_expr.clone()
                + a_limbs[1].clone() * b_limbs[3].clone()
                + a_limbs[2].clone() * b_limbs[2].clone()
                + a_limbs[3].clone() * b_limbs[2].clone()
                + a_limbs[2].clone() * b_limbs[3].clone()
                + a_limbs[3].clone() * b_limbs[2].clone()
                + a_limbs[3].clone() * b_limbs[3].clone();

            let check_a = t0.expr() + t1.expr() * pow_of_two::<F>(64) + c_lo
                - (d_lo + carry_lo_expr.clone() * pow_of_two::<F>(128));
            let check_b = t2.expr() + t3.expr() * pow_of_two::<F>(64) + c_hi + carry_lo_expr
                - (d_hi + carry_hi_expr * pow_of_two::<F>(128));

            let range_check_64_expr = range_check_64_config.map(|(config_a, config_b)| {
                config_a.is_lt(meta, None) + config_b.is_lt(meta, None)
            });
            let range_check_128_expr = range_check_128_config.map(|(config_c, config_d)| {
                config_c.is_lt(meta, None) + config_d.is_lt(meta, None)
            });
            let range_check_expr =
                or::expr(range_check_64_expr.into_iter().chain(range_check_128_expr));

            [check_a, check_b, range_check_expr]
                .into_iter()
                .map(move |poly| q_enable.clone() * poly)
        });

        let range_check_64 =
            range_check_64_config.map(|(a, b)| (LtChip::construct(a), LtChip::construct(b)));
        let range_check_128 =
            range_check_128_config.map(|(c, d)| (LtChip::construct(c), LtChip::construct(d)));

        MulAddConfig {
            col0,
            col1,
            col2,
            col3,
            col4,
            overflow,
            u16_table,
            range_check_64,
            range_check_128,
        }
    }

    /// Construct the MulAdd chip given a configuration.
    pub fn construct(config: MulAddConfig<F>) -> Self {
        Self { config }
    }

    /// Assign witness data to the MulAdd chip.
    pub fn assign(
        &self,
        region: &mut Region<'_, F>,
        offset: usize,
        words: [Word; 4],
    ) -> Result<(), Error> {
        let (a, b, c, d) = (words[0], words[1], words[2], words[3]);

        let a_limbs = split_u256_limb64(&a);
        let b_limbs = split_u256_limb64(&b);
        let (c_lo, c_hi) = split_u256(&c);
        let (d_lo, d_hi) = split_u256(&d);

        let t0 = a_limbs[0] * b_limbs[0];
        let t1 = a_limbs[0] * b_limbs[1] + a_limbs[1] * b_limbs[0];
        let t2 = a_limbs[0] * b_limbs[2] + a_limbs[1] * b_limbs[1] + a_limbs[2] * b_limbs[0];
        let t3 = a_limbs[0] * b_limbs[3]
            + a_limbs[1] * b_limbs[2]
            + a_limbs[2] * b_limbs[1]
            + a_limbs[3] * b_limbs[0];

        let (c_lo_minus_d_lo, _) = c_lo.overflowing_sub(d_lo);
        let temp = t0 + (t1 << 64);
        let (carry_lo, _) = temp.overflowing_add(c_lo_minus_d_lo);
        let carry_lo = carry_lo >> 128;
        let (carry_lo_minus_d_hi, _) = carry_lo.overflowing_sub(d_hi);
        let temp = t2 + (t3 << 64) + c_hi;
        let (carry_hi, _) = temp.overflowing_add(carry_lo_minus_d_hi);
        let carry_hi = carry_hi >> 128;

        // a limbs.
        for (i, (column, value)) in [
            self.config.col0,
            self.config.col1,
            self.config.col2,
            self.config.col3,
        ]
        .into_iter()
        .zip(a_limbs)
        .enumerate()
        {
            let a = F::from(value.as_u64());
            region.assign_advice(
                || format!("a limb ({i})"),
                column,
                offset,
                || Value::known(a),
            )?;
            self.config.range_check_64[i]
                .0
                .assign(region, offset, F::from(u64::MAX), a)?;
        }
        region.assign_advice(
            || format!("unused col: {offset}"),
            self.config.col4,
            offset,
            || Value::known(F::zero()),
        )?;

        // b limbs.
        for (i, (column, value)) in [
            self.config.col0,
            self.config.col1,
            self.config.col2,
            self.config.col3,
        ]
        .into_iter()
        .zip(b_limbs)
        .enumerate()
        {
            let b = F::from(value.as_u64());
            region.assign_advice(
                || format!("b limb ({i})"),
                column,
                offset + 1,
                || Value::known(b),
            )?;
            self.config.range_check_64[i]
                .1
                .assign(region, offset, F::from(u64::MAX), b)?;
        }
        region.assign_advice(
            || format!("unused col {}", offset + 1),
            self.config.col4,
            offset + 1,
            || Value::known(F::zero()),
        )?;

        // c_lo, c_hi, d_lo, d_hi.
        region.assign_advice(
            || "c_lo",
            self.config.col0,
            offset + 2,
            || Value::known(F::from_u128(c_lo.as_u128())),
        )?;
        region.assign_advice(
            || "c_hi",
            self.config.col1,
            offset + 2,
            || Value::known(F::from_u128(c_hi.as_u128())),
        )?;
        self.config.range_check_128[0].0.assign(
            region,
            offset,
            F::from_u128(u128::MAX),
            F::from_u128(c_lo.as_u128()),
        )?;
        self.config.range_check_128[1].0.assign(
            region,
            offset,
            F::from_u128(u128::MAX),
            F::from_u128(c_hi.as_u128()),
        )?;
        region.assign_advice(
            || "d_lo",
            self.config.col2,
            offset + 2,
            || Value::known(F::from_u128(d_lo.as_u128())),
        )?;
        region.assign_advice(
            || "d_hi",
            self.config.col3,
            offset + 2,
            || Value::known(F::from_u128(d_hi.as_u128())),
        )?;
        self.config.range_check_128[0].1.assign(
            region,
            offset,
            F::from_u128(u128::MAX),
            F::from_u128(d_lo.as_u128()),
        )?;
        self.config.range_check_128[1].1.assign(
            region,
            offset,
            F::from_u128(u128::MAX),
            F::from_u128(d_hi.as_u128()),
        )?;
        region.assign_advice(
            || format!("unused col: {}", offset + 2),
            self.config.col4,
            offset + 2,
            || Value::known(F::zero()),
        )?;

        // carry_lo.
        for (i, ((col, rot), val)) in [
            (self.config.col0, offset + 3),
            (self.config.col1, offset + 3),
            (self.config.col2, offset + 3),
            (self.config.col3, offset + 3),
            (self.config.col4, offset + 3),
            (self.config.col0, offset + 4),
            (self.config.col1, offset + 4),
            (self.config.col2, offset + 4),
            (self.config.col3, offset + 4),
        ]
        .into_iter()
        .zip(carry_lo.to_le_bytes().iter())
        .enumerate()
        {
            let v = F::from(*val as u64);
            region.assign_advice(|| format!("carry lo ({i})"), col, rot, || Value::known(v))?;
        }
        region.assign_advice(
            || format!("unused col: {}", offset + 4),
            self.config.col4,
            offset + 4,
            || Value::known(F::zero()),
        )?;

        // carry_hi.
        for (i, ((col, rot), val)) in [
            (self.config.col0, offset + 5),
            (self.config.col1, offset + 5),
            (self.config.col2, offset + 5),
            (self.config.col3, offset + 5),
            (self.config.col4, offset + 5),
            (self.config.col0, offset + 6),
            (self.config.col1, offset + 6),
            (self.config.col2, offset + 6),
            (self.config.col3, offset + 6),
        ]
        .into_iter()
        .zip(carry_hi.to_le_bytes().iter())
        .enumerate()
        {
            let v = F::from(*val as u64);
            region.assign_advice(|| format!("carry hi ({i})"), col, rot, || Value::known(v))?;
        }
        region.assign_advice(
            || format!("unused col: {}", offset + 6),
            self.config.col4,
            offset + 6,
            || Value::known(F::zero()),
        )?;

        Ok(())
    }

    /// Annotates columns of this gadget embedded within a circuit region.
    pub fn annotate_columns_in_region(&self, region: &mut Region<F>, prefix: &str) {
        [
            (self.config.col0, "GADGET_MUL_ADD_col0"),
            (self.config.col1, "GADGET_MUL_ADD_col1"),
            (self.config.col2, "GADGET_MUL_ADD_col2"),
            (self.config.col3, "GADGET_MUL_ADD_col3"),
            (self.config.col4, "GADGET_MUL_ADD_col4"),
        ]
        .iter()
        .for_each(|(col, ann)| region.name_column(|| format!("{prefix}_{ann}"), *col));
    }
}

#[cfg(test)]
mod test {
    use std::marker::PhantomData;

    use eth_types::{Field, Word};
    use halo2_proofs::{
        circuit::{SimpleFloorPlanner, Value},
        dev::MockProver,
        halo2curves::bn256::Fr as Fp,
        plonk::{Circuit, Selector},
    };
    use rand::Rng;

    use crate::mul_add::{MulAddChip, MulAddConfig};

    macro_rules! try_test_circuit {
        ($values:expr) => {{
            let k = 17;
            let circuit = TestCircuit::<Fp> {
                values: $values,
                _marker: PhantomData,
            };
            let prover = MockProver::<Fp>::run(k, &circuit, vec![]).unwrap();
            prover.assert_satisfied_par()
        }};
    }

    macro_rules! try_test_circuit_error {
        ($values:expr) => {{
            let k = 17;
            let circuit = TestCircuit::<Fp> {
                values: $values,
                _marker: PhantomData,
            };
            let prover = MockProver::<Fp>::run(k, &circuit, vec![]).unwrap();
            assert!(prover.verify().is_err());
        }};
    }

    pub(crate) fn rand_bytes_array<const N: usize>() -> [u8; N] {
        [(); N].map(|_| rand::random())
    }

    pub(crate) fn rand_word() -> Word {
        Word::from_big_endian(&rand_bytes_array::<32>())
    }

    #[test]
    fn mul_over_rows() {
        #[derive(Clone)]
        struct TestCircuitConfig<F> {
            q_enable: Selector,
            mul_config: MulAddConfig<F>,
        }

        #[derive(Clone, Default)]
        struct TestCircuit<F> {
            /// (a, b, d) tuples for a * b == d (mod 2^256).
            values: Vec<(Word, Word, Word)>,
            _marker: PhantomData<F>,
        }

        impl<F: Field> Circuit<F> for TestCircuit<F> {
            type Config = TestCircuitConfig<F>;
            type FloorPlanner = SimpleFloorPlanner;

            fn configure(meta: &mut halo2_proofs::plonk::ConstraintSystem<F>) -> Self::Config {
                let q_enable = meta.complex_selector();
                let u16_table = meta.lookup_table_column();
                let mul_config =
                    MulAddChip::configure(meta, |meta| meta.query_selector(q_enable), u16_table);
                Self::Config {
                    q_enable,
                    mul_config,
                }
            }

            fn synthesize(
                &self,
                config: Self::Config,
                mut layouter: impl halo2_proofs::circuit::Layouter<F>,
            ) -> Result<(), halo2_proofs::plonk::Error> {
                let chip = MulAddChip::construct(config.mul_config);

                layouter.assign_table(
                    || "u16 table",
                    |mut table| {
                        for i in 0..=65535 {
                            table.assign_cell(
                                || format!("u16 table row {i}"),
                                chip.config.u16_table,
                                i,
                                || Value::known(F::from(i as u64)),
                            )?;
                        }
                        Ok(())
                    },
                )?;

                layouter.assign_region(
                    || "witness",
                    |mut region| {
                        let mut offset = 0;
                        for (a, b, d) in self.values.iter() {
                            config.q_enable.enable(&mut region, offset)?;
                            chip.assign(&mut region, offset, [*a, *b, Word::zero(), *d])?;
                            offset += 7
                        }
                        Ok(())
                    },
                )
            }

            fn without_witnesses(&self) -> Self {
                Self::default()
            }
        }

        let mut rng = rand::thread_rng();
        let n = 100;
        let mut values = Vec::with_capacity(n);
        for _ in 0..n {
            let a = rand_word();
            let b = rand_word();
            let (d, _) = a.overflowing_mul(b);
            values.push((a, b, d));
        }

        try_test_circuit!(values.clone());
        try_test_circuit_error!(values
            .into_iter()
            .map(|(a, b, d)| {
                if rng.gen::<bool>() {
                    (a, b, d + 1)
                } else {
                    (a, b, d - 1)
                }
            })
            .collect::<Vec<(Word, Word, Word)>>());
    }
}
