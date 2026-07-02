#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrobberFrame {
    pub symbol: &'static str,
    pub energy: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrobberKind {
    ToolPulse,
    BrailleOrbit,
}

const TOOL_PULSE_FRAMES: &[ThrobberFrame] = &[
    ThrobberFrame {
        symbol: "⠁",
        energy: 0,
    },
    ThrobberFrame {
        symbol: "⠃",
        energy: 1,
    },
    ThrobberFrame {
        symbol: "⠇",
        energy: 2,
    },
    ThrobberFrame {
        symbol: "⠧",
        energy: 3,
    },
    ThrobberFrame {
        symbol: "⠷",
        energy: 2,
    },
    ThrobberFrame {
        symbol: "⠿",
        energy: 1,
    },
];

const BRAILLE_ORBIT_SYMBOLS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LIGHT_SWEEP_TAIL: usize = 5;

impl ThrobberKind {
    pub fn frame(self, animation_tick: u64) -> ThrobberFrame {
        match self {
            Self::ToolPulse => frame_at(TOOL_PULSE_FRAMES, animation_tick / 4),
            Self::BrailleOrbit => symbol_frame_at(BRAILLE_ORBIT_SYMBOLS, animation_tick, 2),
        }
    }
}

pub fn light_sweep_distance(index: usize, char_count: usize, animation_tick: u64) -> Option<usize> {
    if char_count == 0 {
        return None;
    }

    let head = (animation_tick as usize) % (char_count + LIGHT_SWEEP_TAIL);
    Some(head.abs_diff(index))
}

fn frame_at(frames: &[ThrobberFrame], index: u64) -> ThrobberFrame {
    frames[(index as usize) % frames.len()]
}

fn symbol_frame_at(symbols: &[&'static str], index: u64, energy: u8) -> ThrobberFrame {
    ThrobberFrame {
        symbol: symbols[(index as usize) % symbols.len()],
        energy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_pulse_preserves_existing_cadence() {
        assert_eq!(ThrobberKind::ToolPulse.frame(0).symbol, "⠁");
        assert_eq!(ThrobberKind::ToolPulse.frame(12).symbol, "⠧");
        assert_eq!(ThrobberKind::ToolPulse.frame(12).energy, 3);
    }

    #[test]
    fn orbit_throbber_advances_every_tick() {
        assert_eq!(ThrobberKind::BrailleOrbit.frame(0).symbol, "⠋");
        assert_eq!(ThrobberKind::BrailleOrbit.frame(1).symbol, "⠙");
    }

    #[test]
    fn light_sweep_distance_uses_tail_padding() {
        assert_eq!(light_sweep_distance(0, 4, 0), Some(0));
        assert_eq!(light_sweep_distance(0, 4, 4), Some(4));
        assert_eq!(light_sweep_distance(0, 0, 0), None);
    }
}
