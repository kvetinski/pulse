use rand::Rng;

use crate::domain::ports::PhoneGenerator;

pub struct RandomUsPhoneGenerator;

impl PhoneGenerator for RandomUsPhoneGenerator {
    fn generate(&self) -> String {
        let mut rng = rand::rng();
        let n1: u16 = rng.random_range(200..999);
        let n2: u16 = rng.random_range(100..999);
        let n3: u16 = rng.random_range(1000..9999);
        format!("+1{n1}{n2}{n3}")
    }
}
