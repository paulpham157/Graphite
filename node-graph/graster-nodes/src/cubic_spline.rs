#[derive(Debug)]
pub struct CubicSplines {
	pub x: [f32; 4],
	pub y: [f32; 4],
}

impl CubicSplines {
	pub fn solve(&self) -> [f32; 4] {
		let (x, y) = (&self.x, &self.y);

		// Build an augmented matrix to solve the system of equations using Gaussian elimination
		let mut augmented_matrix = [
			[
				2. / (x[1] - x[0]),
				1. / (x[1] - x[0]),
				0.,
				0.,
				// |
				3. * (y[1] - y[0]) / ((x[1] - x[0]) * (x[1] - x[0])),
			],
			[
				1. / (x[1] - x[0]),
				2. * (1. / (x[1] - x[0]) + 1. / (x[2] - x[1])),
				1. / (x[2] - x[1]),
				0.,
				// |
				3. * ((y[1] - y[0]) / ((x[1] - x[0]) * (x[1] - x[0])) + (y[2] - y[1]) / ((x[2] - x[1]) * (x[2] - x[1]))),
			],
			[
				0.,
				1. / (x[2] - x[1]),
				2. * (1. / (x[2] - x[1]) + 1. / (x[3] - x[2])),
				1. / (x[3] - x[2]),
				// |
				3. * ((y[2] - y[1]) / ((x[2] - x[1]) * (x[2] - x[1])) + (y[3] - y[2]) / ((x[3] - x[2]) * (x[3] - x[2]))),
			],
			[
				0.,
				0.,
				1. / (x[3] - x[2]),
				2. / (x[3] - x[2]),
				// |
				3. * (y[3] - y[2]) / ((x[3] - x[2]) * (x[3] - x[2])),
			],
		];

		// Gaussian elimination: forward elimination
		for row in 0..4 {
			let pivot_row_index = (row..4)
				.max_by(|&a_row, &b_row| {
					augmented_matrix[a_row][row]
						.abs()
						.partial_cmp(&augmented_matrix[b_row][row].abs())
						.unwrap_or(core::cmp::Ordering::Equal)
				})
				.unwrap();

			// Swap the current row with the row that has the largest pivot element
			augmented_matrix.swap(row, pivot_row_index);

			// Eliminate the current column in all rows below the current one
			for row_below_current in row + 1..4 {
				assert!(augmented_matrix[row][row].abs() > f32::EPSILON);

				let scale_factor = augmented_matrix[row_below_current][row] / augmented_matrix[row][row];
				for col in row..5 {
					augmented_matrix[row_below_current][col] -= augmented_matrix[row][col] * scale_factor
				}
			}
		}

		// Gaussian elimination: back substitution
		let mut solutions = [0.; 4];
		for col in (0..4).rev() {
			assert!(augmented_matrix[col][col].abs() > f32::EPSILON);

			solutions[col] = augmented_matrix[col][4] / augmented_matrix[col][col];

			for row in (0..col).rev() {
				augmented_matrix[row][4] -= augmented_matrix[row][col] * solutions[col];
				augmented_matrix[row][col] = 0.;
			}
		}

		solutions
	}

	pub fn interpolate(&self, input: f32, solutions: &[f32]) -> f32 {
		if input <= self.x[0] {
			return self.y[0];
		}
		if input >= self.x[self.x.len() - 1] {
			return self.y[self.x.len() - 1];
		}

		// Find the segment that the input falls between
		let mut segment = 1;
		while self.x[segment] < input {
			segment += 1;
		}
		let segment_start = segment - 1;
		let segment_end = segment;

		// Calculate the output value using quadratic interpolation
		let input_value = self.x[segment_start];
		let input_value_prev = self.x[segment_end];
		let output_value = self.y[segment_start];
		let output_value_prev = self.y[segment_end];
		let solutions_value = solutions[segment_start];
		let solutions_value_prev = solutions[segment_end];

		let output_delta = solutions_value_prev * (input_value - input_value_prev) - (output_value - output_value_prev);
		let solution_delta = (output_value - output_value_prev) - solutions_value * (input_value - input_value_prev);

		let input_ratio = (input - input_value_prev) / (input_value - input_value_prev);
		let prev_output_ratio = (1. - input_ratio) * output_value_prev;
		let output_ratio = input_ratio * output_value;
		let quadratic_ratio = input_ratio * (1. - input_ratio) * (output_delta * (1. - input_ratio) + solution_delta * input_ratio);

		let result = prev_output_ratio + output_ratio + quadratic_ratio;
		result.clamp(0., 1.)
	}
}
