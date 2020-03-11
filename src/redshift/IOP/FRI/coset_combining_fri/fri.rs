use crate::pairing::ff::PrimeField;
use crate::multicore::*;
use crate::SynthesisError;

use super::*;
use super::query_producer::*;
use std::convert::From;
use crate::redshift::IOP::oracle::*;
use crate::redshift::fft::cooley_tukey_ntt::log2_floor;


impl<F: PrimeField, Params: FriParams<F>, O: Oracle<F>, C: Channel<F, Input = O::Commitment>> FriIop<F, Params, O, C> {
    type Params = Params;
    type OracleType = O;
    type Channel = C;
    type Proof = FriProof<F, Self::OracleType>;

    fn proof_from_lde<T: FriPrecomputations<F>
    >(
        lde_values: &Polynomial<F, Values>,
        lde_factor: usize,
        precomputations: &T,
        worker: &Worker,
        channel: &mut Self::Channel,
        params: &Self::Params
    ) -> Result<FriProofPrototype<F, Self::Oracle>, SynthesisError> {
        Self::proof_from_lde_by_values(
            lde_values, 
            lde_factor,
            precomputations,
            worker,
            channel,
            params
        )
    }

    fn prototype_into_proof(
        prototype: FriProofPrototype<F, Self::Oracle>,
        natural_first_element_indexes: Vec<usize>,
        params: &Self::Params
    ) -> Result<Self::Params::Proof, SynthesisError> {
        prototype.produce_proof(natural_first_element_indexes)
    }

    fn get_fri_challenges(
        proof: &Self::Proof,
        channel: &mut Self::Channel,
        params: &Self::Params
    ) -> Vec<F> {
        let mut fri_challenges = vec![];

        for commitment in proof.commitments.iter() {
            let iop_challenge = {
                channel.consume(&commitment);
                channel.get_field_element()
            };

            fri_challenges.push(iop_challenge);
        }

        fri_challenges
    }

    fn verify_proof_with_challenges(
        proof: &Self::Proof,
        natural_element_indexes: Vec<usize>,
        fri_challenges: &[F],
        params: &Self::Params
    ) -> Result<bool, SynthesisError> {
        Self::verify_proof_queries(proof, natural_element_indexes, fri_challenges, params)
    }

    pub fn proof_from_lde_by_values<T: FriPrecomputations<F>>
    (
        // we assume lde_values to be in bitreversed order
        lde_values: &Polynomial<F, Values>,
        lde_factor: usize,
        precomputations: &C,
        worker: &Worker,
        channel: &mut Self::Channel,
        params: &Self::Params
    ) -> Result<FriProofPrototype<F, Self::Oracle>, SynthesisError> {
        
        let initial_domain_size = lde_values.size();
        assert_eq!(precomputations.domain_size(), initial_domain_size);

        let mut two = F::one();
        two.double();
        let two_inv = two.inverse().expect("should exist");
        let final_degree_plus_one = params.OUTPUT_POLY_DEGREE + 1;
        
        assert!(final_degree_plus_one.is_power_of_two());
        assert!(lde_factor.is_power_of_two());

        let initial_degree_plus_one = initial_domain_size / lde_factor;
        let wrapping_factor = params.COLLAPSING_FACTOR;
        let num_steps = log2_floor(initial_degree_plus_one / final_degree_plus_one) / log2_floor(wrapping_factor) as u32;
    
        let mut oracles = Vec::with_capacity(num_steps);
        let mut challenges = Vec::with_capacity(num_steps);
        let mut intermediate_values = Vec::with_capacity(num_steps);

        //TODO: locate all of them in LDE order
        let oracle_params = <Self::OracleType as Oracle<F>>::Params::from(1 << wrapping_factor);
        let initial_oracle = <Self::OracleType as Oracle<F>>::create(lde_values.as_ref(), &oracle_params);
        oracles.push(initial_oracle);
        
        // if we would precompute all N we would have
        // [0, N/2, N/4, 3N/4, N/8, N/2 + N/8, N/8 + N/4, N/8 + N/4 + N/2, ...]
        // but we only precompute half of them and have
        // [0, N/4, N/8, N/8 + N/4, ...]

        let omegas_inv_bitreversed: &[F] = precomputations.omegas_inv_bitreversed();
        let this_domain_size = initial_domain_size;
        let mut values_slice = lde_values.as_ref();
        
        for fri_step in 0..num_steps {
            let next_domain_size = this_domain_size / wrapping_factor;
            let mut next_values = vec![F::zero(); next_domain_size];

            channel.consume(oracles.last().as_ref().expect("should not be empty").get_commitment());
            let mut challenge = channel.get_field_element();
            challenges.push(challenge.clone());

            // we combine like this with FRI trees being aware of the FRI computations
            //            next_value(omega**)
            //          /                     \
            //    intermediate(omega*)       intermediate(-omega*)
            //    /           \                   /            \
            // this(omega)   this(-omega)     this(omega')    this(-omega')
            // 
            // so omega* = omega^2i. omega' = sqrt(-omega^2i) = sqrt(omega^(N/2 + 2i)) = omega^N/4 + i
            // 
            // we expect values to come bitreversed, so this(omega) and this(-omega) are always adjustent to each other
            // because in normal emumeration it would be elements b0XYZ and b1XYZ, and now it's bZYX0 and bZYX1
            // 
            // this(omega^(N/4 + i)) for b00YZ has a form b01YZ, so bitreversed it's bZY00 and bZY10
            // this(-omega^(N/4 + i)) obviously has bZY11, so they are all near in initial values

            worker.scope(next_values.len(), |scope, chunk| {
                for (i, v) in next_values.chunks_mut(chunk).enumerate() {

                    scope.spawn(move |_| {
                        let initial_k = i*chunk;
                        let mut this_level_values = Vec::with_capacity(wrapping_factor);
                        let mut next_level_values = vec![F::zero(); wrapping_factor];
                        for (j, v) in v.iter_mut().enumerate() {
                            let batch_id = initial_k + j;
                            let values_offset = batch_id*wrapping_factor;
                            for wrapping_step in 0..wrapping_factor {
                                let base_omega_idx = (batch_id * wrapping_factor) >> (1 + wrapping_step);
                                let expected_this_level_values = wrapping_factor >> wrapping_step;
                                let expected_next_level_values = wrapping_factor >> (wrapping_step + 1);
                                let inputs = if wrapping_step == 0 {
                                    &values_slice[values_offset..(values_offset + wrapping_factor)]
                                } else {
                                    &this_level_values[..expected_this_level_values]
                                };

                                // imagine first FRI step, first wrapping step
                                // in values we have f(i), f(i + N/2), f(i + N/4), f(i + N/4 + N/2), f(i + N/8), ...
                                // so we need to use omega(i) for the first pair, omega(i + N/4) for the second, omega(i + N/8)
                                // on the next step we would have f'(2i), f'(2i + N/2), f'(2i + N/4), f'(2i + N/4 + N/2)
                                // so we would have to pick omega(2i) and omega(2i + N/4)
                                // this means LSB is always the same an only depend on the pair_idx below
                                // MSB is more tricky
                                // for a batch number 0 we have i = 0
                                // for a batch number 1 due to bitreverse we have index equal to b000001xxx where LSB are not important in the batch
                                // such value actually gives i = bxxx100000 that is a bitreverse of the batch number with proper number of bits
                                // due to precomputed omegas also being bitreversed we just need a memory location b000001xxx >> 1

                                debug_assert_eq!(inputs.len() / 2, expected_next_level_values);

                                for (pair_idx, (pair, o)) in inputs.chunks(2).zip(next_level_values[..expected_next_level_values].iter_mut()).enumerate() {
                                    debug_assert!(base_omega_idx & pair_idx == 0);
                                    let omega_idx = base_omega_idx + pair_idx;
                                    let omega_inv = omegas_inv_bitreversed[omega_idx];
                                    let f_at_omega = pair[0];
                                    let f_at_minus_omega = pair[1];
                                    let mut v_even_coeffs = f_at_omega;
                                    v_even_coeffs.add_assign(&f_at_minus_omega);

                                    let mut v_odd_coeffs = f_at_omega;
                                    v_odd_coeffs.sub_assign(&f_at_minus_omega);
                                    v_odd_coeffs.mul_assign(&omega_inv);

                                    let mut tmp = v_odd_coeffs;
                                    tmp.mul_assign(&challenge);
                                    tmp.add_assign(&v_even_coeffs);
                                    tmp.mul_assign(&two_inv);

                                    *o = tmp;
                                }

                                this_level_values.clear();
                                this_level_values.clone_from(&next_level_values);
                                challenge.double();
                            }

                            *v = next_level_values[0];
                        }
                    });
                }
            });

            
            if fri_step < num_steps - 1 {
                this_domain_size = next_domain_size;
                let intermediate_oracle = <Self::OracleType as Oracle<F>>::create(next_values.as_ref(), &oracle_params);
                oracles.push(intermediate_oracle);             
            } 

            let next_values_as_poly = Polynomial::from_values(next_values)?;
            intermediate_values.push(next_values_as_poly);

            values_slice = intermediate_values.last().expect("is something").as_ref();      
        }

        assert_eq!(challenges.len(), num_steps);
        assert_eq!(oracles.len(), num_steps);
        assert_eq!(intermediate_values.len(), num_steps);

        let mut final_poly_values = Polynomial::from_values(values_slice.to_vec())?;
        final_poly_values.bitreverse_enumeration(&worker);
        let final_poly_coeffs = final_poly_values.icoset_fft_for_generator(&worker, &F::multiplicative_generator());
       
        let mut final_poly_coeffs = final_poly_coeffs.into_coeffs();

        let mut degree = final_poly_coeffs.len() - 1;
        for c in final_poly_coeffs.iter().rev() {
            if c.is_zero() {
                degree -= 1;
            } else {
                break
            }
        }

        assert!(degree < final_degree_plus_one, "polynomial degree is too large, coeffs = {:?}", final_poly_coeffs);

        final_poly_coeffs.truncate(final_degree_plus_one);

        Ok(FriProofPrototype {
            oracles,
            challenges,
            intermediate_values,
            final_coefficients: final_poly_coeffs,
        })
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn test_bench_fri_with_coset_combining() {
        use crate::ff::Field;
        use crate::ff::PrimeField;
        use rand::{XorShiftRng, SeedableRng, Rand, Rng};
        use crate::redshift::partial_reduction_field::PartialTwoBitReductionField;
        use crate::redshift::partial_reduction_field::Fr;
        use crate::redshift::polynomials::*;
        use std::time::Instant;
        use crate::multicore::*;
        use crate::redshift::fft::cooley_tukey_ntt::{CTPrecomputations, BitReversedOmegas};
        use crate::redshift::IOP::FRI::coset_combining_fri::precomputation::*;
        use crate::redshift::IOP::FRI::coset_combining_fri::FriPrecomputations;
        use crate::redshift::IOP::FRI::coset_combining_fri::fri;

        const SIZE: usize = 1024;
        let worker = Worker::new_with_cpus(1);

        let rng = &mut XorShiftRng::from_seed([0x3dbe6259, 0x8d313d76, 0x3237db17, 0xe5bc0654]);
        let coeffs = (0..SIZE).map(|_| Fr::rand(rng)).collect::<Vec<_>>();
    
        let poly = Polynomial::<Fr, _>::from_coeffs(coeffs).unwrap();
        let precomp = BitReversedOmegas::<Fr>::new_for_domain_size(poly.size());
        let start = Instant::now();
        let coset_factor = Fr::multiplicative_generator();
        let eval_result = poly.bitreversed_lde_using_bitreversed_ntt(&worker, 16, &precomp, &coset_factor).unwrap();
        println!("LDE with factor 16 for size {} bitreversed {:?}", SIZE, start.elapsed());

        let fri_precomp = <OmegasInvBitreversed::<Fr> as FriPrecomputations<Fr>>::new_for_domain_size(eval_result.size());


        let fri_proto = CosetCombiningFriIop::<Fr>::proof_from_lde(
            &eval_result, 
            16, 
            2, 
            &fri_precomp, 
            &worker, 
            &mut transcript,
            &params
        ).expect("FRI must succeed");
    }

    #[test]
    #[should_panic]
    fn test_invalid_eval_fri_with_coset_combining() {
        use crate::ff::Field;
        use crate::ff::PrimeField;
        use rand::{XorShiftRng, SeedableRng, Rand, Rng};
        use crate::plonk::transparent_engine::proth_engine::Fr;
        use crate::plonk::transparent_engine::PartialTwoBitReductionField;
        use crate::plonk::polynomials::*;
        use std::time::Instant;
        use crate::multicore::*;
        use crate::plonk::commitments::transparent::utils::*;
        use crate::plonk::fft::cooley_tukey_ntt::{CTPrecomputations, BitReversedOmegas};
        use crate::plonk::commitments::transparent::fri::coset_combining_fri::FriPrecomputations;
        use crate::plonk::commitments::transparent::fri::coset_combining_fri::fri::*;
        use crate::plonk::commitments::transparent::fri::coset_combining_fri::precomputation::OmegasInvBitreversed;
        use crate::plonk::commitments::transcript::*;

        const SIZE: usize = 1024;

        let mut transcript = Blake2sTranscript::<Fr>::new();

        let worker = Worker::new_with_cpus(1);

        let rng = &mut XorShiftRng::from_seed([0x3dbe6259, 0x8d313d76, 0x3237db17, 0xe5bc0654]);
        let coeffs = (0..SIZE).map(|_| Fr::rand(rng)).collect::<Vec<_>>();
    
        let poly = Polynomial::<Fr, _>::from_coeffs(coeffs).unwrap();
        let precomp = BitReversedOmegas::<Fr>::new_for_domain_size(poly.size());
        let start = Instant::now();
        let coset_factor = Fr::multiplicative_generator();
        let mut eval_result = poly.bitreversed_lde_using_bitreversed_ntt(&worker, 16, &precomp, &coset_factor).unwrap();
        eval_result.as_mut()[1].sub_assign(&Fr::one());
        println!("LDE with factor 16 for size {} bitreversed {:?}", SIZE, start.elapsed());

        let fri_precomp = <OmegasInvBitreversed::<Fr> as FriPrecomputations<Fr>>::new_for_domain_size(eval_result.size());

        let params = CosetParams::<Fr> {
            cosets_schedule: vec![3, 3, 3],
            coset_factor: coset_factor
        };

        let fri_proto = CosetCombiningFriIop::<Fr>::proof_from_lde(
            &eval_result, 
            16, 
            2, 
            &fri_precomp, 
            &worker, 
            &mut transcript,
            &params
        ).expect("FRI must succeed");
    }
}