module.exports = grammar({
  name: 'nim',

  extras: $ => [
    /\s/,
    $.comment,
  ],

  conflicts: $ => [
    [$.return_statement],
    [$.call, $._expr],
  ],

  rules: {
    source_file: $ => repeat(choice(
      $.proc_declaration,
      $.func_declaration,
      $._statement
    )),

    proc_declaration: $ => prec.right(seq(
      'proc', field('name', $.identifier),
      optional(field('parameters', $.parameters)),
      optional(seq(':', field('return_type', $.type))),
      optional(seq('=', field('body', $.body)))
    )),

    func_declaration: $ => prec.right(seq(
      'func', field('name', $.identifier),
      optional(field('parameters', $.parameters)),
      optional(seq(':', field('return_type', $.type))),
      optional(seq('=', field('body', $.body)))
    )),

    parameters: $ => seq(
      '(',
      optional(seq($.parameter, repeat(seq(',', $.parameter)))),
      ')'
    ),

    parameter: $ => seq(
      field('name', $.identifier),
      optional(seq(':', $._expr))
    ),

    type: $ => /[a-zA-Z0-9_:]+/,

    body: $ => prec.right(repeat1($._statement)),

    _statement: $ => choice(
      $.echo_statement,
      $.local_decl,
      $.expression_statement,
      $.assignment,
      $.return_statement
    ),

    echo_statement: $ => seq('echo', field('value', $._expr)),

    local_decl: $ => seq(
      choice('let', 'var', 'const'),
      field('name', $.identifier),
      optional(seq(':', $._expr)),
      '=',
      field('value', $._expr)
    ),

    expression_statement: $ => field('value', $._expr),

    assignment: $ => seq(
      field('name', $.identifier),
      '=',
      field('value', $._expr)
    ),

    return_statement: $ => seq('return', optional($._expr)),

    _expr: $ => choice(
      $.binary_expression,
      $.parenthesized_expression,
      $.string,
      $.int,
      $.call,
      $.identifier
    ),

    parenthesized_expression: $ => seq(
      '(', field('value', $._expr), ')'
    ),

    binary_expression: $ => prec.left(seq(
      field('left', $._expr),
      choice('+', '-', '*', '/', '%', '&', '|', '^', '<<', '>>', '==', '!=', '<', '<=', '>', '>='),
      field('right', $._expr)
    )),

    string: $ => /"([^"\\]|\\.)*"/,

    int: $ => /-?\d+/,

    call: $ => prec.right(seq(
      optional(seq($.identifier, '.')),
      field('name', $.identifier),
      optional(field('arguments', $.arguments))
    )),

    arguments: $ => seq(
      '(',
      optional(seq($._argument, repeat(seq(',', $._argument)))),
      ')'
    ),

    _argument: $ => choice(
      $._expr,
      seq(field('name', $.identifier), ':', field('value', $._expr))
    ),

    identifier: $ => /[a-zA-Z_][a-zA-Z0-9_]*/,

    comment: $ => seq('#', /[^\n]*/),
  },
});
