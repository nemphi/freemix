#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AlphaMode {
    Straight,
    Premultiplied,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ColorPrimaries {
    Bt601,
    Bt709,
    Bt2020,
    DisplayP3,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransferFunction {
    Linear,
    Srgb,
    Bt709,
    Bt1886,
    Hlg,
    Pq,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MatrixCoefficients {
    Identity,
    Bt601,
    Bt709,
    Bt2020NonConstant,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SignalRange {
    Full,
    Limited,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChromaLocation {
    Left,
    Center,
    TopLeft,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ColorMetadata {
    pub primaries: ColorPrimaries,
    pub transfer: TransferFunction,
    pub matrix: MatrixCoefficients,
    pub range: SignalRange,
    pub chroma_location: ChromaLocation,
}

impl Default for ColorMetadata {
    fn default() -> Self {
        Self {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt1886,
            matrix: MatrixCoefficients::Bt709,
            range: SignalRange::Limited,
            chroma_location: ChromaLocation::Left,
        }
    }
}
