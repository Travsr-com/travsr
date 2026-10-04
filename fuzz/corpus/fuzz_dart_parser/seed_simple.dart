import 'dart:async';

abstract class Greeter {
  String greet(String name);
}

class Hello implements Greeter {
  final String prefix;

  Hello(this.prefix);

  @override
  String greet(String name) => '$prefix $name';
}

void main() {
  print(Hello('hello').greet('world'));
}
