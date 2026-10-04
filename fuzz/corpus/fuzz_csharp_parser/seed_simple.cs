using System;

namespace Acme.App
{
    public interface IGreeter
    {
        string Greet(string name);
    }

    public class Greeter : IGreeter
    {
        public string Greet(string name) => $"hello {name}";
    }
}
