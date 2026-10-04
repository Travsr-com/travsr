package com.acme.app

import scala.collection.mutable

trait Greeter {
  def greet(name: String): String
}

class Hello(prefix: String) extends Greeter {
  def greet(name: String): String = s"$prefix $name"
}

object Main {
  def run(): Unit = println(new Hello("hello").greet("world"))
}
