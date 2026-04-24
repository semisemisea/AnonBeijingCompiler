#[derive(Debug, Clone)]
pub struct Integer {
    value: i32,
}

impl Integer {
    pub fn value(&self) -> i32 {
        self.value
    }
}

#[derive(Debug, Clone)]
pub struct Float {
    value: f32,
}

impl Float {
    pub fn value(&self) -> f32 {
        self.value
    }
}
