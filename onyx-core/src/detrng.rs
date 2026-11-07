//! PRNG determinista del cerebro.
//! No criptográfico. Solo para reproducibilidad.
//!
//! Un generador por tick, compartido por los sistemas en orden de ejecución.
//! La semilla debe formar parte del estado recuperable de la aplicación.
//! Misma semilla, mismo tick y mismas llamadas producen la misma secuencia.
//! La mezcla separa semilla y tick, pero no garantiza ausencia de colisiones.

#[derive(Clone, Debug)]
pub struct DetRng {
    state: u64,
}

impl DetRng {
    /// Mezcla semilla y tick por separado y evita el estado cero.
    pub fn from_seed_and_tick(seed: u64, tick: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;

        let mut w = tick.wrapping_add(0x6A09E667F3BCC909);
        w = (w ^ (w >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        w = (w ^ (w >> 27)).wrapping_mul(0x94D049BB133111EB);
        w ^= w >> 31;

        let state = z.wrapping_add(w).wrapping_mul(0x9E3779B97F4A7C15);
        Self {
            state: if state == 0 {
                0xDEADBEEFCAFEBABE
            } else {
                state
            },
        }
    }

    /// Siguiente entero de la secuencia xorshift64*.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Siguiente valor en el intervalo [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Rango [low, high). Usa rechazo para evitar sesgo del módulo.
    /// Si low >= high, devuelve low sin consumir la secuencia.
    pub fn gen_range(&mut self, low: u64, high: u64) -> u64 {
        if low >= high {
            return low;
        }
        let range = high - low;
        let threshold = u64::MAX - (u64::MAX % range);
        loop {
            let r = self.next_u64();
            if r < threshold {
                return low + (r % range);
            }
        }
    }
}
