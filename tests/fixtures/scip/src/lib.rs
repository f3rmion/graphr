pub trait Alpha {
    fn alpha_only(&self) -> u32;
}

pub trait Beta {
    fn beta_only(&self) -> u32;
}

pub fn bounded<S: Alpha + Beta>(s: &S) -> u32 {
    s.alpha_only()
}
