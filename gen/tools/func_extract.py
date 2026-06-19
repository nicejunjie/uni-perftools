#!/usr/bin/env python3

import re
import sys
import os

import re

def preprocess_fixed_form(content):
    # Remove inline comments
    content = re.sub(r'!.*$', '', content, flags=re.MULTILINE)

    lines = content.split('\n')
    processed_content = ""

    for line in lines:
        # Skip comment lines, empty lines, and lines with only whitespace
        if not line.strip() or line[0] in 'Cc*':
            continue

        # Check for continuation
        if len(line) > 5 and line[5] != ' ':
            # Continuation line: append without newline
            processed_content += ' ' + line[6:].lstrip().rstrip()
        else:
            # Regular line: append with newline
            processed_content += '\n' + line.rstrip()

    return processed_content

def preprocess_free_form(content):
    # Remove full-line comments and empty lines
    lines = [line for line in content.split('\n') if line.strip() and not line.strip().startswith('!')]

    processed_content = ""

    for line in lines:
        # Remove inline comments
        line = re.sub(r'!.*$', '', line)
        
        # Process the line
        tmp_line = line.rstrip()
        
        # Check for trailing '&'
        if not tmp_line.endswith('&'):
            tmp_line += '\n'
        else:
            tmp_line = tmp_line[:-1]  # Remove trailing '&'
        
        # Check for leading '&'
        if tmp_line.lstrip().startswith('&'):
            tmp_line = tmp_line.lstrip()[1:]  # Remove leading '&' and preceding whitespace
        
        processed_content += tmp_line

    return processed_content


def print_debug_info(content):
    print("\n--- Processed File Content (for debugging) ---")
    print(content)
    print("--- End of Processed File Content ---\n")

def split_fortran_code(content):
    # Regular expression to match function or subroutine declarations
    pattern = r'(?im)^\s*(?:.*\s+)?(function|subroutine)\s+\w+'

    
    # Find all matches
    matches = list(re.finditer(pattern, content))
    
    # Split the content into individual functions/subroutines
    functions = []
    for i in range(len(matches)):
        start = matches[i].start()
        end = matches[i+1].start() if i+1 < len(matches) else len(content)
        function_code = content[start:end].strip()
        functions.append(function_code)
        
    #   print(f"\nDebug: Function/Subroutine {i+1}")
    #   print("-" * 50)
    #   print((function_code))
    #   print("=" * 50)
    
    return functions

def process_replace(code):
    # Replace kind specifications and standardize on double precision and double complex
    code = re.sub(r'real\s*\(\s*kind\s*=\s*1\s*\)', 'real', code, flags=re.IGNORECASE)
    code = re.sub(r'real\s*\(\s*kind\s*=\s*2\s*\)', 'double precision', code, flags=re.IGNORECASE)
    code = re.sub(r'real\s*\(\s*kind\s*=\s*4\s*\)', 'real', code, flags=re.IGNORECASE)
    code = re.sub(r'real\s*\(\s*kind\s*=\s*8\s*\)', 'double precision', code, flags=re.IGNORECASE)
    code = re.sub(r'real\s*\*\s*8', 'double precision', code, flags=re.IGNORECASE)
    code = re.sub(r'complex\s*\(\s*kind\s*=\s*1\s*\)', 'complex', code, flags=re.IGNORECASE)
    code = re.sub(r'complex\s*\(\s*kind\s*=\s*2\s*\)', 'double complex', code, flags=re.IGNORECASE)
    code = re.sub(r'complex\s*\(\s*kind\s*=\s*4\s*\)', 'complex', code, flags=re.IGNORECASE)
    code = re.sub(r'complex\s*\(\s*kind\s*=\s*8\s*\)', 'double complex', code, flags=re.IGNORECASE)
    code = re.sub(r'complex\s*\*\s*16', 'double complex', code, flags=re.IGNORECASE)

    code = re.sub(r'end\s+function\s*\w*', 'end', code, flags=re.IGNORECASE)
    code = re.sub(r'end\s+subroutine\s*\w*', 'end', code, flags=re.IGNORECASE)

    #print(code)

    return code

def extract_signature(function_code):

    pattern = r'kind\s*\('
    issue = bool(re.search(pattern, function_code))

    pattern = r'(?i)(?:(.*?)\s+)?(function|subroutine)\s+(\w+)\s*\(([^)]*)\)'

    match = re.search(pattern, function_code)
    
    if match:
        return_type, type_, name, params = match.groups()

        
        type_keywords = ['integer', 'real', 'double precision', 'complex', 'double complex', 'logical', 'character']
        
        var_types = {}
        
        declaration_pattern = r'(?im)^\s*(' + '|'.join(type_keywords) + r')(?:\s*::)?\s*(.*?)$'
        declaration_lines = re.findall(declaration_pattern, function_code)
        
        for type_keyword, var_list in declaration_lines:
            c_type = {
                'integer': 'libprof_int_t',
                'real': 'float',
                'double precision': 'double',
                'complex': 'libprof_fcomplex_t',
                'double complex': 'libprof_dcomplex_t',
                'logical': 'bool',
                'character': 'char'
            }.get(type_keyword.lower(), 'ATTENTION')
            
            vars = re.findall(r'\w+(?:\s*\([^)]*\))?', var_list)
            for var in vars:
                var_name = var.split('(')[0].strip()
                if '(' in var:
                   var_types[var_name.lower()] = c_type + '*'
                else:
                  #var_types[var_name.lower()] = c_type # add * to all since we are doing fortran to C
                   var_types[var_name.lower()] = c_type + '*'
        
        param_list = [p.strip() for p in params.split(',')] if params else []
        c_style_params = []
        
        for param in param_list:
            c_type = var_types.get(param.lower(), 'ATTENTION')
            c_style_params.append(f"{c_type} {param}")
        
        if type_.lower() == 'function':
            if return_type:
                c_return_type = {
                    'integer': 'libprof_int',
                    'real': 'float',
                    'double precision': 'double',
                    'complex': 'libprof_fcomplex',
                    'double complex': 'libprof_dcomplex',
                    'logical': 'bool',
                    'character': 'char',
                    'void': 'void'
                }.get(return_type.lower().strip(), 'ATTENTION')
            else:
                c_return_type = 'ATTENTION'
        else:
            c_return_type = 'void'
        
        if (issue): 
            c_style_signature = f"KIND_ATTENTION {c_return_type} {name}({', '.join(c_style_params)})"
        else:
            c_style_signature = f"{c_return_type} {name}({', '.join(c_style_params)})"
        return c_style_signature
    else:
        return "Error: Signature not found"

def extract_signatures(content):
    content = process_replace(content)
    functions = split_fortran_code(content)
    signatures = []

    for i, func in enumerate(functions, 1):
        signature = extract_signature(func)
        signatures.append(signature)
   
    return signatures



def main():
    if len(sys.argv) != 2:
        print("Usage: python script_name.py <fortran_file_path>")
        sys.exit(1)

    file_path = sys.argv[1]
    try:
        with open(file_path, 'r') as file:
            content = file.read()

        _, file_extension = os.path.splitext(file_path)
        if file_extension.lower() == '.f':
            processed_content =  preprocess_fixed_form(content)
        else:
            processed_content =  preprocess_free_form(content)

#       print_debug_info(processed_content)

        signatures = extract_signatures(processed_content)

        if signatures:
           print("\nConverted C-style function signature:")
           for signature in signatures:
              if 'error' in signature.lower():
                 print(f"'{file_path}': ERROR, No function/subroutine signatures found.")
              else: 
                 print(signature)
        else:
            print(f"'{file_path}': ERROR.")


    except FileNotFoundError:
        print(f"Error: File '{file_path}' not found.")
    except Exception as e:
        print(f"An error occurred: {str(e)}")

if __name__ == "__main__":
    main()
