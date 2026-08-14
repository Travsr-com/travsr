// #479 golden fixture — C#. Attributes are decisive, so all four cases fit one
// file (no test-path convention needed).
using NUnit.Framework;

[TestFixture]
public class CalibrationTests
{
    [Test]
    public void Calibrates() {}   // EntryPoint: [Test] attribute.

    private void Helper() {}       // Support: helper in the [TestFixture] scope.
}

public class Calibrator
{
    public void CalibrateFloors() {}    // None: ordinary production code.

    // Adversarial None: a test-ish name with no attribute, outside any scope.
    public void TestConnectionPool() {}
}
